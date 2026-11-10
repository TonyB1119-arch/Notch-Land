//! The launcher: press the hotkey, type, press Enter. Two glass boxes as in Tuna: the thing you
//! found on the left, what will happen to it on the right. The list of matches stays hidden until
//! you press the Down arrow.
//!
//! It finds apps (Start menu and Store apps), files and folders (Desktop, Documents, Downloads,
//! Pictures, Videos, Music and the folders in `[launcher] folders`), answers sums like `12*(3+4)`,
//! opens web addresses and falls back to a web search.
//!
//! Look: dark glass. The picture behind the window is copied when it opens, blurred once and drawn
//! inside the card, so the blur works everywhere and moves with the card. Everything animates (the
//! window opening and closing, the list dropping down, the highlight sliding, the underline growing);
//! the animation timer runs only while something is moving.
//!
//! Three modes, switched with Tab while it is open: searching, the clipboard history (Win+C) and a
//! one-question chat with Gemini (Win+G): the question on the left, the answer on the right.
//!
//! Cost: nothing exists until the hotkey is pressed for the first time. The window, its drawing
//! surface and the lists are built then; 20 seconds after it closes the drawing memory is handed
//! back, and after five minutes everything is gone again. While it is closed it uses no CPU.

use std::collections::HashMap;
use std::mem::size_of;
use std::os::windows::fs::MetadataExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use windows::core::{w, BOOL, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_EXCLUDED_FROM_PEEK, DWMWA_USE_HOSTBACKDROPBRUSH,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND,
};
use windows::Win32::Graphics::Gdi::{
    CreateRectRgn, GetMonitorInfoW, MonitorFromPoint, SetWindowRgn, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::UI::Composition::Compositor;

use crate::config::{GeminiCfg, LauncherCfg};
use crate::gfx::{Gfx, Icon, ImageData, Painter, Res, TextStyle};
use crate::host::Host;
use crate::procs;
use crate::modules::weather::Brief;
use crate::sys::{self, clip, dragdrop};
use crate::util::{app_dir, Color, Rect, SendHwnd};

const CLASS: PCWSTR = w!("DynamicNotch.Launcher");
const T_ANIM: usize = 1;
const T_ICON: usize = 2;
const T_TRIM: usize = 3;
const T_FREE: usize = 4;
const T_FLASH: usize = 5;
/// Live blur: the screen behind is looked at again.
const T_LIVE: usize = 6;
/// Colour picker: the pixels around the pointer are looked at again.
const T_PICK: usize = 7;
/// The apps are looked for again a while after one was uninstalled.
const T_RESCAN: usize = 8;
/// Window switcher: while Alt is held it is looked at, so letting go of it opens the chosen window.
const T_ALT: usize = 9;
/// A background scan has finished.
const WM_INDEX: u32 = WM_APP + 31;
/// Gemini has sent more of the answer.
const WM_AI: u32 = WM_APP + 32;
/// Today's exchange rates have arrived.
const WM_RATES: u32 = WM_APP + 33;
/// The programs have been looked at again (Kill mode).
const WM_PROCS: u32 = WM_APP + 34;
/// A picture of a window (window switcher) is ready.
const WM_PREVIEW: u32 = WM_APP + 35;
/// The mouse was clicked on something other than the launcher (sent by the click watcher).
const WM_OUTSIDE: u32 = WM_APP + 36;

// -- layout (dips) -------------------------------------------------------------------------------------
const PAD: f32 = 14.0;
const BOX: f32 = 172.0;
const GAP: f32 = 10.0;
const W: f32 = PAD * 2.0 + BOX * 2.0 + GAP;
/// The Kill mode has a third box: the card grows by this much on each side.
const E: f32 = (BOX + GAP) * 0.5;
/// The window is as wide as the card with three boxes; the card is narrower in the other modes.
const W3: f32 = W + BOX + GAP;
const FRAME_H: f32 = BOX + PAD * 2.0;
const HEAD: f32 = 30.0;
const ROW: f32 = 46.0;
/// Rows of the list that show at once.
const VIS: usize = 6;
/// The list floats as its own glass panel this far below the two boxes.
const LIST_GAP: f32 = 10.0;
const LIST_Y: f32 = FRAME_H + LIST_GAP;
/// Rows the menu of things to do can show (a list of matches shows `VIS`).
const VIS_MAX: usize = 8;
const FULL_H: f32 = LIST_Y + HEAD + VIS_MAX as f32 * ROW + 10.0;
const CARD_R: f32 = 34.0;
const LIST_R: f32 = 28.0;
const BOX_R: f32 = 24.0;
/// The window switcher's cards: size, space between them, and how many show at once.
const SW_CW: f32 = 168.0;
const SW_CH: f32 = 160.0;
const SW_GAP: f32 = 10.0;
const SW_R: f32 = 20.0;
const SW_VIS: usize = 3;

/// Matches kept per search.
const KEEP: usize = 40;
const MAX_FILES: usize = 30_000;
const MAX_DEPTH: u8 = 5;
const SKIP_DIRS: [&str; 3] = ["node_modules", "__pycache__", "site-packages"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    App,
    Folder,
    File,
    Calc,
    Url,
    Web,
    Weather,
    Day,
    /// An item of the clipboard history (its `path` is `clip:<id>`).
    Clip,
    /// A command for the computer (`idx` is its place in `conv::SYSTEM`).
    System,
    /// An emoji or symbol (its `path` is the character).
    Emoji,
    /// A running program (its `path` is where it lives, or just its file name).
    Process,
    /// A command from the Run box's history (its `path` is the command).
    Run,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::App => "Application",
            Kind::Folder => "Folder",
            Kind::File => "File",
            Kind::Calc => "Calculator",
            Kind::Url => "Link",
            Kind::Web => "Web search",
            Kind::Weather => "Weather",
            Kind::Day => "Forecast",
            Kind::Clip => "Clipboard",
            Kind::System => "System",
            Kind::Emoji => "Emoji",
            Kind::Process => "Program",
            Kind::Run => "Command",
        }
    }
    fn has_icon(self) -> bool {
        matches!(self, Kind::App | Kind::Folder | Kind::File | Kind::Process)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Act {
    Open,
    Admin,
    Reveal,
    CopyPath,
    CopyResult,
    OpenLink,
    Search,
    Forecast,
    Quit,
    Restart,
    ForceQuit,
    Paste,
    CopyClip,
    DeleteClip,
    /// Pick an app to open the file with, or a folder to move or copy it to.
    OpenWith,
    MoveTo,
    CopyTo,
    /// Run a system command.
    Run,
    /// Take the app off the computer.
    Uninstall,
    PasteEmoji,
    CopyEmoji,
    /// End the program.
    Kill,
}

impl Act {
    fn label(self) -> &'static str {
        match self {
            Act::Open => "Open",
            Act::Admin => "Run as administrator",
            Act::Reveal => "Show in folder",
            Act::CopyPath => "Copy path",
            Act::CopyResult => "Copy result",
            Act::OpenLink => "Open link",
            Act::Search => "Search the web",
            Act::Forecast => "Open forecast",
            Act::Quit => "Quit",
            Act::Restart => "Restart",
            Act::ForceQuit => "Force quit",
            Act::Paste => "Paste",
            Act::CopyClip => "Copy",
            Act::DeleteClip => "Remove from history",
            Act::OpenWith => "Open with\u{2026}",
            Act::MoveTo => "Move to\u{2026}",
            Act::CopyTo => "Copy to\u{2026}",
            Act::Run => "Run",
            Act::Uninstall => "Uninstall",
            Act::PasteEmoji => "Paste",
            Act::CopyEmoji => "Copy",
            Act::Kill => "Kill",
        }
    }
    fn icon(self) -> Icon {
        match self {
            Act::Open => Icon::Send,
            Act::Admin => Icon::Bolt,
            Act::Reveal => Icon::Folder,
            Act::CopyPath | Act::CopyResult => Icon::Copy,
            Act::OpenLink => Icon::Link,
            Act::Search => Icon::Globe,
            Act::Forecast => Icon::Cloud,
            Act::Quit => Icon::Close,
            Act::Restart => Icon::Refresh,
            Act::ForceQuit => Icon::Stop,
            Act::Paste => Icon::Clipboard,
            Act::CopyClip => Icon::Copy,
            Act::DeleteClip => Icon::Trash,
            Act::OpenWith => Icon::Grid,
            Act::MoveTo => Icon::Folder,
            Act::CopyTo | Act::CopyEmoji => Icon::Copy,
            Act::Run => Icon::Bolt,
            Act::Uninstall => Icon::Trash,
            Act::PasteEmoji => Icon::Clipboard,
            Act::Kill => Icon::Stop,
        }
    }
}

/// What can be done with a match; the first one is what Enter does.
fn actions(h: &Hit) -> &'static [Act] {
    match h.kind {
        Kind::App if h.path.starts_with("shell:") => &[Act::Open, Act::Uninstall],
        Kind::App => &[Act::Open, Act::Admin, Act::Reveal, Act::Quit, Act::Restart, Act::ForceQuit, Act::Uninstall],
        Kind::Folder | Kind::File => &[Act::Open, Act::Reveal, Act::CopyPath, Act::OpenWith, Act::MoveTo, Act::CopyTo],
        Kind::Calc => &[Act::CopyResult],
        Kind::Url => &[Act::OpenLink, Act::CopyPath],
        Kind::Web => &[Act::Search],
        Kind::Weather | Kind::Day => &[Act::Forecast],
        Kind::Clip => &[Act::Paste, Act::CopyClip, Act::DeleteClip],
        Kind::System => &[Act::Run],
        Kind::Emoji => &[Act::PasteEmoji, Act::CopyEmoji],
        Kind::Process => &[Act::Kill],
        Kind::Run => &[Act::Run],
    }
}

/// One thing that can be found. Files keep only their path (the name is its tail).
struct Entry {
    path: Box<str>,
    label: Option<Box<str>>,
    at: u32,
    kind: Kind,
}

impl Entry {
    fn name(&self) -> &str {
        match &self.label {
            Some(l) => &**l,
            None => &self.path[self.at as usize..],
        }
    }
}

/// A match as shown.
#[derive(Clone)]
struct Hit {
    name: String,
    /// The second line: a path, the sum, the address.
    detail: String,
    /// What gets opened (and the key of its icon).
    path: String,
    kind: Kind,
    /// Matched letters, as byte ranges of `name`.
    spans: Vec<(usize, usize)>,
    /// Which day of the forecast (Weather rows).
    idx: usize,
}

/// What the launcher is for: finding things, or going through what was copied.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Search,
    Clip,
    Ai,
    Notes,
    Emoji,
    /// Ending programs.
    Kill,
    /// The Run box (Win+R): one wide box, never reached with Tab.
    Run,
    /// The window switcher (Alt+Tab): a row of the open windows, never reached with Tab.
    Windows,
}

impl Mode {
    /// The next mode (or the one before) when Tab (Shift+Tab) is pressed.
    fn cycle(self, back: bool) -> Mode {
        const ORDER: [Mode; 6] = [Mode::Search, Mode::Clip, Mode::Ai, Mode::Kill, Mode::Emoji, Mode::Notes];
        let i = ORDER.iter().position(|m| *m == self).unwrap_or(0);
        ORDER[(i + if back { 5 } else { 1 }) % 6]
    }
}

/// How far the Gemini question has got.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AiState {
    Idle,
    Waiting,
    Done,
    Failed,
}

/// The answer as the background request fills it in.
struct AiShared {
    /// Which question this belongs to (an answer to an older one is ignored).
    seq: u64,
    text: String,
    done: bool,
    err: Option<String>,
}

/// The Gemini mode: one question, one answer.
struct Ai {
    seq: u64,
    state: AiState,
    text: String,
    /// How far the answer is scrolled, and how far it can be (set while drawing).
    scroll: f32,
    max_scroll: f32,
    /// How the question (left box) and the answer (right box) are set: size and place glide.
    q: Glide,
    a: Glide,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    shared: Arc<Mutex<AiShared>>,
}

impl Ai {
    fn new() -> Self {
        Self {
            seq: 0,
            state: AiState::Idle,
            text: String::new(),
            scroll: 0.0,
            max_scroll: 0.0,
            q: Glide::new(Q_SIZES[0]),
            a: Glide::new(A_SIZES[0]),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            shared: Arc::new(Mutex::new(AiShared { seq: 0, text: String::new(), done: false, err: None })),
        }
    }

    /// Stop the request that is running and forget the answer.
    fn clear(&mut self) {
        self.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        self.seq += 1;
        self.state = AiState::Idle;
        self.text = String::new();
        self.scroll = 0.0;
        self.max_scroll = 0.0;
        self.a = Glide::new(A_SIZES[0]);
    }

    /// The text eases toward its size and place.
    fn glide(&mut self, dt: f32, snap: bool) {
        self.q.step(dt, snap);
        self.a.step(dt, snap);
    }

    fn moving(&self) -> bool {
        self.q.moving() || self.a.moving()
    }
}

/// Where a piece of text sits in its box: the size and top edge as drawn, where they are heading, whether
/// it is centred (short text) or left-aligned (long text), and which text that was worked out for.
struct Glide {
    sz: f32,
    y: f32,
    goal: (f32, f32),
    center: bool,
    fit_for: u64,
}

impl Glide {
    fn new(sz: f32) -> Self {
        Self { sz, y: GLIDE_Y0, goal: (sz, GLIDE_Y0), center: true, fit_for: 0 }
    }

    fn step(&mut self, dt: f32, snap: bool) {
        if snap {
            (self.sz, self.y) = self.goal;
        } else {
            self.sz = logic::follow(self.sz, self.goal.0, dt, 14.0, 0.05);
            self.y = logic::follow(self.y, self.goal.1, dt, 14.0, 0.05);
        }
    }

    fn moving(&self) -> bool {
        (self.sz - self.goal.0).abs() > 0.05 || (self.y - self.goal.1).abs() > 0.05
    }

    /// Take a new fit worked out by `fit_text` for the text with this hash.
    fn set(&mut self, hash: u64, fit: (f32, f32, bool)) {
        self.fit_for = hash;
        self.goal = (fit.0, fit.1);
        self.center = fit.2;
    }
}

/// Text sizes to try, biggest first: the question, the answer and the clipboard text start big and
/// centred, and get smaller as they grow.
const Q_SIZES: [f32; 6] = [34.0, 28.0, 23.0, 19.0, 16.0, 14.0];
const A_SIZES: [f32; 6] = [24.0, 20.0, 17.0, 15.0, 13.0, 12.0];
const C_SIZES: [f32; 6] = [20.0, 17.0, 15.0, 13.0, 12.0, 11.0];
/// Where the top of a one-line text starts (the boxes are 144 high inside).
const GLIDE_Y0: f32 = 48.0;

fn text_hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hs = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut hs);
    hs.finish() | 1
}

/// The biggest of `sizes` at which `text` still fits a `w` x `h` box: (that size, where its top edge
/// goes, centred or not). It sits in the middle (and centred) while it fits, left-aligned from the top
/// once it has grown; a question keeps its end in view when even the smallest size is too long.
fn fit_text(
    p: &mut Painter,
    text: &str,
    w: f32,
    h: f32,
    sizes: &[f32],
    weight: u16,
    keep_end: bool,
) -> (f32, f32, bool) {
    if text.is_empty() {
        return (sizes[0], GLIDE_Y0, true);
    }
    let mut used = 0.0;
    for (i, &sz) in sizes.iter().enumerate() {
        used = p.measure_wrapped(text, &TextStyle::new(sz, white(0.9)).weight(weight).wrap(), w);
        if used <= h {
            return (sz, (h - used) * 0.5, i <= 2);
        }
    }
    (sizes[sizes.len() - 1], if keep_end { h - used } else { 0.0 }, false)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Subject,
    Action,
}

/// How long the old contents take to swell and shrink away when Tab changes the mode (seconds).
const SWITCH_OUT: f32 = 0.11;

/// A mode change in progress: the contents of both boxes swell a little and shrink away into the middle of
/// their box (`t` runs 0 to 1); then `to` takes over and its contents grow back out of the middle.
struct Switch {
    to: Mode,
    t: f32,
}

/// Pixels across the little magnifier of the colour picker (odd, so one is in the middle).
const PICK_N: i32 = 11;

/// The colour picker: what is under the pointer, live.
struct Pick {
    /// The pixels around the pointer (`PICK_N` x `PICK_N`, 0x00RRGGBB); the middle one is the pick.
    cells: Vec<u32>,
    /// Which way of writing it is chosen on the right: 0 HEX, 1 RGB, 2 HSL.
    row: usize,
}

impl Pick {
    fn rgb(&self) -> (u8, u8, u8) {
        let v = self.cells.get(self.cells.len() / 2).copied().unwrap_or(0);
        (((v >> 16) & 0xFF) as u8, ((v >> 8) & 0xFF) as u8, (v & 0xFF) as u8)
    }

    /// The colour written out the three ways: (what is shown, what is copied).
    fn texts(&self) -> [(String, String); 3] {
        let c = self.rgb();
        let hex = conv::hex(c);
        let (h, sa, l) = conv::hsl(c);
        [
            (hex.clone(), hex),
            (format!("{}, {}, {}", c.0, c.1, c.2), format!("rgb({}, {}, {})", c.0, c.1, c.2)),
            (format!("{h}\u{b0}, {sa}%, {l}%"), format!("hsl({h}, {sa}%, {l}%)")),
        ]
    }
}

type Slot = Arc<Mutex<Option<Vec<Entry>>>>;
/// Pictures of windows, handed over by the thread that takes them.
type PrevSlot = Arc<Mutex<Vec<(isize, ImageData)>>>;

pub struct Launcher {
    hwnd: HWND,
    host: Host,
    res: Res,
    cfg: LauncherCfg,
    /// Taking input (false while it fades out).
    open: bool,
    /// The window is on screen (also while it fades out).
    visible: bool,
    mode: Mode,
    /// Shown for a moment by a combo (it puts itself away).
    flashing: bool,
    /// What to show instead of search results (a combo's target).
    pinned: Option<Hit>,
    /// A combo's flash is one box with the icon (no search box beside it).
    one_box: bool,
    /// Which version of the dock's icons the picture cache holds.
    icon_gen: u32,
    free_me: bool,
    scale: f32,
    win_px: (i32, i32, i32, i32),
    region_h: i32,
    region_x: i32,
    // the Kill mode: the programs as last looked at, and the numbers the gauges glide to
    sampler: Option<procs::Sampler>,
    snap: Option<procs::Snap>,
    procs_slot: procs::Slot,
    /// How far the card has grown to three boxes (0 to 1).
    kw: f32,
    cpu_s: f32,
    ram_s: f32,
    /// How far the Kill box has turned red (asked once).
    arm_t: f32,
    // what can be found
    apps: Vec<Entry>,
    files: Vec<Entry>,
    apps_ready: bool,
    /// When the apps were last looked for (they are looked for again now and then).
    apps_at: Instant,
    apps_slot: Slot,
    files_slot: Slot,
    files_started: bool,
    usage: HashMap<String, u32>,
    /// What was chosen for what was typed: (typed text, path) -> times. It makes the same typing find the same thing first.
    learn: HashMap<(String, String), u32>,
    icons: HashMap<String, Option<ImageData>>,
    // what the user is doing
    query: String,
    hits: Vec<Hit>,
    sel: usize,
    top: usize,
    act: usize,
    focus: Focus,
    list_open: bool,
    high: Option<u16>,
    mouse: (f32, f32),
    // animation: all of these move toward where they belong while the timer runs
    timer_on: bool,
    last_tick: Instant,
    /// Window opening (1) and closing (0).
    anim: f32,
    /// Height of the dropped-down list, in dips.
    lh: f32,
    /// Underline growing, the icon popping in (box 1 and box 2): 0 to 1.
    ul: f32,
    pop_s: f32,
    pop_a: f32,
    /// Which box has the focus (0 = left, 1 = right).
    focus_t: f32,
    /// The highlighted row, as a (moving) row number.
    sel_y: f32,
    /// The blurred picture of the screen behind the window.
    blur_img: Option<ImageData>,
    wx: Option<std::rc::Rc<crate::modules::weather::Brief>>,
    scratch: Vec<usize>,
    ai: Ai,
    /// The contents sliding in after Tab: 0 to 1, and the way they come from (1 or -1).
    sw: f32,
    /// A Tab pressed: the old contents are shrinking away; the next mode takes over when they have.
    switch: Option<Switch>,
    /// The picture behind the glass is renewed ten times a second while the launcher is open (a video
    /// behind it keeps moving). The window is hidden from screen copies for that.
    live: bool,
    /// Picking the target of an action: the action and the thing it is for.
    pending: Option<(Act, Hit)>,
    /// A command that asked for a second Enter.
    armed: Option<String>,
    /// The colour picker, while it is open.
    pick: Option<Pick>,
    /// Quick notes (newest first) and how they are shown.
    notes: Vec<(u64, String)>,
    notes_text: String,
    /// The first lines of the text file on the left.
    texts: HashMap<String, String>,
    /// The commands run from the Run box, newest first.
    run_hist: Vec<String>,
    /// The last command could not be started.
    run_err: bool,
    /// How much of the card is the one Run box (0 = two boxes, 1 = one box).
    rw: f32,
    /// The window switcher: the windows, the chosen one, how far the row is scrolled (in cards), their
    /// pictures, where those arrive, and how often Tab (or an arrow) was pressed while Alt was held.
    wn: Vec<crate::switcher::Win>,
    wn_sel: usize,
    wn_x: f32,
    wn_prev: HashMap<isize, ImageData>,
    wn_slot: PrevSlot,
    wn_cancel: Arc<AtomicBool>,
    wn_tabs: u32,
}

impl Launcher {
    pub fn new(gfx: &Gfx, compositor: Compositor) -> windows::core::Result<Self> {
        let hwnd = create_window()?;
        let host = Host::new_shared(hwnd, gfx, 64, 64, compositor)?;
        let res = Res::new(gfx)?;
        let apps_slot: Slot = Arc::new(Mutex::new(None));
        spawn_apps(apps_slot.clone(), SendHwnd::new(hwnd));
        Ok(Self {
            hwnd,
            host,
            res,
            cfg: LauncherCfg::default(),
            open: false,
            visible: false,
            mode: Mode::Search,
            flashing: false,
            pinned: None,
            one_box: false,
            icon_gen: 0,
            free_me: false,
            scale: 1.0,
            win_px: (0, 0, 0, 0),
            region_h: -1,
            region_x: 0,
            sampler: None,
            snap: None,
            procs_slot: Arc::new(Mutex::new(None)),
            kw: 0.0,
            cpu_s: 0.0,
            ram_s: 0.0,
            arm_t: 0.0,
            apps: Vec::new(),
            files: Vec::new(),
            apps_ready: false,
            apps_at: Instant::now(),
            apps_slot,
            files_slot: Arc::new(Mutex::new(None)),
            files_started: false,
            usage: load_usage(),
            learn: load_learn(),
            icons: HashMap::new(),
            query: String::new(),
            hits: Vec::new(),
            sel: 0,
            top: 0,
            act: 0,
            focus: Focus::Subject,
            list_open: false,
            high: None,
            mouse: (-1.0, -1.0),
            timer_on: false,
            last_tick: Instant::now(),
            anim: 0.0,
            lh: 0.0,
            ul: 1.0,
            pop_s: 1.0,
            pop_a: 1.0,
            focus_t: 0.0,
            sel_y: 0.0,
            blur_img: None,
            wx: None,
            scratch: Vec::new(),
            ai: Ai::new(),
            sw: 1.0,
            switch: None,
            live: false,
            pending: None,
            armed: None,
            pick: None,
            notes: Vec::new(),
            notes_text: String::new(),
            texts: HashMap::new(),
            run_hist: Vec::new(),
            run_err: false,
            rw: 0.0,
            wn: Vec::new(),
            wn_sel: 0,
            wn_x: 0.0,
            wn_prev: HashMap::new(),
            wn_slot: Arc::new(Mutex::new(Vec::new())),
            wn_cancel: Arc::new(AtomicBool::new(false)),
            wn_tabs: 0,
        })
    }

    pub fn configure(&mut self, cfg: LauncherCfg) {
        self.cfg = cfg;
    }

    /// The idle timer ran out: the app should drop the launcher.
    pub fn wants_free(&self) -> bool {
        self.free_me
    }

    pub fn recover(&mut self, gfx: &Gfx) {
        let _ = self.host.set_rendering_device(gfx);
        if let Ok(r) = Res::new(gfx) {
            self.res = r;
        }
        self.draw(gfx);
    }

    pub fn toggle(&mut self, gfx: &Gfx, ui_scale: f32) {
        if self.open && self.mode == Mode::Search {
            self.hide();
        } else {
            self.mode = Mode::Search;
            self.show(gfx, ui_scale, true);
        }
    }

    /// The clipboard hotkey: the launcher as a history of what was copied.
    pub fn toggle_clip(&mut self, gfx: &Gfx, ui_scale: f32) {
        if self.open && self.mode == Mode::Clip {
            self.hide();
        } else {
            self.mode = Mode::Clip;
            self.show(gfx, ui_scale, true);
        }
    }

    /// The Gemini hotkey: ask one quick question.
    pub fn toggle_ai(&mut self, gfx: &Gfx, ui_scale: f32) {
        if self.open && self.mode == Mode::Ai {
            self.hide();
        } else {
            self.mode = Mode::Ai;
            self.show(gfx, ui_scale, true);
        }
    }

    /// The Run hotkey: the launcher as one wide box for a command.
    pub fn toggle_run(&mut self, gfx: &Gfx, ui_scale: f32) {
        if self.open && self.mode == Mode::Run {
            self.hide();
        } else {
            self.mode = Mode::Run;
            self.show(gfx, ui_scale, true);
        }
    }

    /// The switcher hotkey (Alt+Tab): the launcher as a row of the open windows. Pressed again while it is open
    /// it steps to the next window (Alt+Shift+Tab: the one before).
    pub fn windows_step(&mut self, gfx: &Gfx, ui_scale: f32, back: bool) {
        if self.open && self.mode == Mode::Windows {
            self.windows_move(gfx, if back { -1 } else { 1 });
            return;
        }
        self.mode = Mode::Windows;
        self.show(gfx, ui_scale, true);
    }

    /// The open windows are listed (the one in front first; the choice starts on the one before it) and their
    /// pictures are taken in the background, the ones next to the choice first.
    fn windows_open(&mut self) {
        self.windows_close();
        self.wn = crate::switcher::list();
        self.wn_sel = if self.wn.len() > 1 { 1 } else { 0 };
        self.wn_x = self.wn_target();
        self.wn_tabs = 0;
        let sel = self.wn_sel;
        let mut order: Vec<(usize, isize, bool)> =
            self.wn.iter().enumerate().map(|(i, w)| (i.abs_diff(sel), w.hwnd, w.minimized)).collect();
        order.sort_by_key(|o| o.0);
        let order: Vec<(isize, bool)> = order.into_iter().map(|o| (o.1, o.2)).collect();
        self.wn_cancel = Arc::new(AtomicBool::new(false));
        spawn_previews(order, self.wn_slot.clone(), self.wn_cancel.clone(), SendHwnd::new(self.hwnd));
        // Alt still held (Alt+Tab): letting go of it opens the chosen window, once Tab was pressed again
        if alt_down() {
            unsafe {
                SetTimer(Some(self.hwnd), T_ALT, 30, None);
            }
        }
    }

    /// Stop fetching pictures and let go of the windows and their pictures.
    fn windows_close(&mut self) {
        self.wn_cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        unsafe {
            let _ = KillTimer(Some(self.hwnd), T_ALT);
        }
        for (_, img) in self.wn_prev.drain() {
            self.res.forget(img.id);
        }
        if let Ok(mut g) = self.wn_slot.lock() {
            g.clear();
        }
        self.wn.clear();
    }

    /// Where the row of cards wants to be scrolled to: the chosen card is in the middle when it can be.
    fn wn_target(&self) -> f32 {
        let n = self.wn.len();
        if n <= SW_VIS {
            0.0
        } else {
            (self.wn_sel as f32 - 1.0).clamp(0.0, (n - SW_VIS) as f32)
        }
    }

    /// Choose the next (`d` = 1) or the previous (`d` = -1) window; after the last comes the first.
    fn windows_move(&mut self, gfx: &Gfx, d: i32) {
        let n = self.wn.len() as i32;
        if n == 0 {
            return;
        }
        self.wn_tabs += 1;
        self.wn_sel = (self.wn_sel as i32 + d).rem_euclid(n) as usize;
        self.kick();
        self.draw(gfx);
    }

    /// Bring the chosen window to the front and put the switcher away.
    fn windows_commit(&mut self) {
        let Some(h) = self.wn.get(self.wn_sel).map(|w| w.hwnd) else { return };
        self.hide();
        sys::activate_window(h);
    }

    fn windows_key(&mut self, gfx: &Gfx, vk: u16, shift: bool) {
        match VIRTUAL_KEY(vk) {
            VK_TAB => self.windows_move(gfx, if shift { -1 } else { 1 }),
            VK_RIGHT | VK_DOWN => self.windows_move(gfx, 1),
            VK_LEFT | VK_UP => self.windows_move(gfx, -1),
            VK_HOME | VK_END => {
                self.wn_sel = if VIRTUAL_KEY(vk) == VK_HOME { 0 } else { self.wn.len().saturating_sub(1) };
                self.kick();
                self.draw(gfx);
            }
            VK_RETURN => self.windows_commit(),
            VK_ESCAPE => self.hide(),
            _ => {}
        }
    }

    /// The card under the pointer.
    fn window_at(&self, x: f32, y: f32) -> Option<usize> {
        let n = self.wn.len();
        if x < -E || x > W + E {
            return None;
        }
        (0..n).find(|&i| sw_rect(i, n, self.wn_x).contains(x, y))
    }

    /// A combo fired: show what is being opened for a moment (0.7 s) without taking the focus away
    /// from it, then put the launcher away again.
    pub fn flash(&mut self, gfx: &Gfx, ui_scale: f32, label: &str, target: &str) {
        let t = target.trim();
        let expanded = crate::dock::expand_env(t);
        let low = t.to_ascii_lowercase();
        let on_disk = Path::new(&expanded);
        let browser = low == "browser" || low == "default browser";
        let (kind, path) = if browser {
            match default_browser_exe() {
                Some(exe) => (Kind::App, exe),
                None => (Kind::Url, self.cfg.search_url.clone()),
            }
        } else if low.starts_with("http://") || low.starts_with("https://") || low.starts_with("www.") {
            (Kind::Url, expanded.clone())
        } else if on_disk.is_dir() {
            (Kind::Folder, expanded.clone())
        } else if low.starts_with("shell:") || low.ends_with(".lnk") || low.ends_with(".exe") || !on_disk.exists() {
            (Kind::App, expanded.clone())
        } else {
            (Kind::File, expanded.clone())
        };
        let name = if !label.trim().is_empty() {
            label.trim().to_string()
        } else if browser {
            "Browser".to_string()
        } else {
            on_disk.file_stem().map(|n| n.to_string_lossy().into_owned()).filter(|n| !n.is_empty()).unwrap_or(expanded)
        };
        self.pinned = Some(Hit { name, detail: String::new(), path, kind, spans: Vec::new(), idx: 0 });
        self.mode = Mode::Search;
        self.show(gfx, ui_scale, false);
        self.flashing = true;
        unsafe {
            SetTimer(Some(self.hwnd), T_FLASH, 700, None);
        }
    }

    fn show(&mut self, gfx: &Gfx, ui_scale: f32, focus: bool) {
        // asked again while it was still fading out: the window is already there
        let again = self.visible;
        self.one_box = self.pinned.is_some();
        self.switch = None;
        self.query.clear();
        self.hits.clear();
        self.sel = 0;
        self.top = 0;
        self.act = 0;
        self.focus = Focus::Subject;
        self.list_open = false;
        self.high = None;
        self.mouse = (-1.0, -1.0);
        self.ai.clear();
        self.ai.q = Glide::new(Q_SIZES[0]);
        self.pending = None;
        self.armed = None;
        self.run_err = false;
        self.stop_pick();
        // apps installed or removed since the last look show up (looked for at most every 20 seconds)
        if self.apps_ready && self.apps_at.elapsed().as_secs() >= 20 {
            self.rescan_apps();
        }
        if self.mode == Mode::Run {
            self.run_hist = load_run_history();
        }
        if self.mode == Mode::Windows {
            self.windows_open();
        }
        if self.mode == Mode::Notes {
            self.notes = load_notes();
            self.notes_text = notes_text(&self.notes);
        }
        // nothing typed yet: the apps used most are what the Down arrow lists (a combo shows its own thing)
        self.sync_procs();
        if let Some(h) = self.pinned.take() {
            self.hits.push(h);
        } else {
            self.search();
        }

        // the clipboard history is a list: it is open from the start
        self.list_open = matches!(self.mode, Mode::Clip | Mode::Emoji) && !self.hits.is_empty();
        if self.cfg.files && !self.files_started && self.mode == Mode::Search {
            self.files_started = true;
            spawn_files(self.files_slot.clone(), SendHwnd::new(self.hwnd), self.cfg.folders.clone());
        }
        if !again {
            if !self.place(ui_scale * self.cfg.size) {
                return;
            }
            // the screen behind the window, copied before the window is shown
            let (x, y, w, h) = self.win_px;
            if let Some(old) = self.blur_img.take() {
                self.res.forget(old.id);
            }
            // the glass blurs with Windows' own live backdrop, always (nothing is copied, so a playing video or
            // anything else behind it keeps moving and the window stays in screenshots); with `live = false`
            // it is one picture taken now
            self.live = self.cfg.glass && self.cfg.live;
            self.blur_img = if self.cfg.glass && !self.live { capture_blur(x, y, w, h, self.cfg.blur) } else { None };
            self.anim = 0.0;
            self.lh = 0.0;
            self.focus_t = 0.0;
            self.sel_y = 0.0;
            self.rw = self.rw_target();
        }
        self.open = true;
        self.visible = true;
        self.ul = 0.0;
        self.pop_s = 0.0;
        self.pop_a = 0.0;
        unsafe {
            let _ = KillTimer(Some(self.hwnd), T_TRIM);
            let _ = KillTimer(Some(self.hwnd), T_FREE);
            let _ = KillTimer(Some(self.hwnd), T_FLASH);
            self.flashing = false;
            if !again {
                // a combo's flash must not take the focus from the thing that is opening
                let _ = ShowWindow(self.hwnd, if focus { SW_SHOW } else { SW_SHOWNOACTIVATE });
            }
            if focus {
                if !SetForegroundWindow(self.hwnd).as_bool() || GetForegroundWindow() != self.hwnd {
                    // Windows only lets the window in front take the focus: a harmless key tap unlocks it
                    let inputs = [key_input(VK_MENU, false), key_input(VK_MENU, true)];
                    SendInput(&inputs, size_of::<INPUT>() as i32);
                    let _ = SetForegroundWindow(self.hwnd);
                }
                let _ = SetFocus(Some(self.hwnd));
            }
        }
        if focus {
            watch_clicks(self.hwnd);
        }
        self.draw(gfx);
    }

    /// Start closing: it fades out, then `finish_hide` puts the window away.
    pub fn hide(&mut self) {
        if !self.open {
            return;
        }
        self.open = false;
        self.flashing = false;
        unwatch_clicks();
        self.switch = None;
        self.stop_pick();
        unsafe {
            let _ = KillTimer(Some(self.hwnd), T_ICON);
            let _ = KillTimer(Some(self.hwnd), T_FLASH);
        }
        self.kick();
    }

    fn finish_hide(&mut self) {
        self.visible = false;
        self.pending = None;
        self.armed = None;
        self.sampler = None;
        self.snap = None;
        self.kw = 0.0;
        self.cpu_s = 0.0;
        self.ram_s = 0.0;
        self.arm_t = 0.0;
        self.windows_close();
        if self.live {
            self.live = false;
            unsafe {
                let _ = KillTimer(Some(self.hwnd), T_LIVE);
                let _ = SetWindowDisplayAffinity(self.hwnd, WDA_NONE);
            }
        }
        self.sw = 1.0;
        self.query.clear();
        self.hits.clear();
        self.ai.clear();
        self.list_open = false;
        self.lh = 0.0;
        self.wx = None;
        if let Some(old) = self.blur_img.take() {
            self.res.forget(old.id);
        }
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
            SetTimer(Some(self.hwnd), T_TRIM, 20_000, None);
        }
    }

    // -- animation ---------------------------------------------------------------------------------------

    /// Ask for animation frames: the timer runs while something is still moving and stops by itself.
    fn kick(&mut self) {
        if self.timer_on {
            return;
        }
        self.timer_on = true;
        self.last_tick = Instant::now();
        unsafe {
            SetTimer(Some(self.hwnd), T_ANIM, 15, None);
        }
    }

    /// How many rows the list shows at once: all of the things to do with a match, or a page of matches.
    fn vis(&self) -> usize {
        if self.focus == Focus::Action {
            VIS_MAX
        } else {
            VIS
        }
    }

    /// The card has three boxes in the Kill mode.
    fn kw_target(&self) -> f32 {
        if matches!(self.mode, Mode::Kill | Mode::Windows) {
            1.0
        } else {
            0.0
        }
    }

    /// The card is one wide box in the Run mode, and in the others until the box with the things to do
    /// with the match is asked for (Right arrow, Ctrl+K, a click on it, a choice or a confirmation).
    fn rw_target(&self) -> f32 {
        let alone = self.cfg.single_box
            && matches!(self.mode, Mode::Search | Mode::Clip | Mode::Emoji)
            && self.focus == Focus::Subject
            && self.pending.is_none()
            && self.armed.is_none()
            && self.pick.is_none();
        if self.mode == Mode::Run || self.one_box || alone {
            1.0
        } else {
            0.0
        }
    }

    /// The left box when it is alone: the whole card, or a square in the middle of it.
    fn solo_rect(&self) -> Rect {
        if self.cfg.square_box && self.mode != Mode::Run {
            Rect::new(PAD + (BOX + GAP) * 0.5, PAD, BOX, BOX)
        } else {
            Rect::new(PAD, PAD, W - 2.0 * PAD, BOX)
        }
    }

    /// How far the glass closes in on each side while the lone box is a square (it opens up again with the second box).
    fn card_inset(&self) -> f32 {
        if self.cfg.square_box && self.mode != Mode::Run {
            (BOX + GAP) * 0.5 * self.rw.clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    /// What the processor and memory gauges are heading for (0 to 1).
    fn gauge_goal(&self) -> (f32, f32) {
        match &self.snap {
            Some(s) if self.mode == Mode::Kill => {
                let ram = if s.ram_total > 0 { s.ram_used as f32 / s.ram_total as f32 } else { 0.0 };
                (s.cpu, ram)
            }
            _ => (0.0, 0.0),
        }
    }

    /// How big the whole card is while it opens (from 85%, swinging a little past 100% on the way) or
    /// closes (it drifts out to 102% as it fades).
    fn open_scale(&self) -> f32 {
        if self.open {
            // it arrives from outside: big and soft at first, settling into place
            1.0 + 0.26 * (1.0 - logic::ease_out(self.anim))
        } else {
            // the pieces fly outward from the middle, faster and faster, as they fade
            let t = 1.0 - self.anim;
            1.0 + 0.40 * t * (0.35 + 0.65 * t)
        }
    }

    /// How tall the list wants to be.
    fn list_target(&self) -> f32 {
        let rows = self.rows_len();
        if self.list_open && rows > 0 {
            HEAD + rows.min(self.vis()) as f32 * ROW + 10.0
        } else {
            0.0
        }
    }

    fn sel_target(&self) -> f32 {
        self.cursor().saturating_sub(self.view_top()) as f32
    }

    fn needs_anim(&self) -> bool {
        let open_to = if self.open { 1.0 } else { 0.0 };
        let focus_to = if self.focus == Focus::Action { 1.0 } else { 0.0 };
        self.anim != open_to
            || (self.lh - self.list_target()).abs() > 0.3
            || self.ul < 1.0
            || self.pop_s < 1.0
            || self.pop_a < 1.0
            || self.sw < 1.0
            || self.switch.is_some()
            || (self.kw - self.kw_target()).abs() > 0.004
            || (self.mode == Mode::Kill
                && ((self.cpu_s - self.gauge_goal().0).abs() > 0.002
                    || (self.ram_s - self.gauge_goal().1).abs() > 0.002
                    || (self.arm_t - if self.armed.is_some() { 1.0 } else { 0.0 }).abs() > 0.01))
            || (matches!(self.mode, Mode::Ai | Mode::Notes | Mode::Run) && self.ai.moving())
            || (self.rw - self.rw_target()).abs() > 0.004
            || (self.mode == Mode::Windows && (self.wn_x - self.wn_target()).abs() > 0.004)
            || (self.focus_t - focus_to).abs() > 0.01
            || (self.sel_y - self.sel_target()).abs() > 0.01
    }

    /// Move everything a little closer to where it belongs. True while something is still moving.
    fn tick(&mut self) -> bool {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f32().min(0.05);
        self.last_tick = now;
        let snap = !self.cfg.animate;
        let open_to = if self.open { 1.0 } else { 0.0 };
        let dur = if self.open { 0.22 } else { 0.17 };
        self.anim = if snap { open_to } else { logic::toward(self.anim, open_to, dt / dur) };
        let lt = self.list_target();
        self.lh = if snap { lt } else { logic::follow(self.lh, lt, dt, 15.0, 0.3) };
        self.ul = if snap { 1.0 } else { logic::toward(self.ul, 1.0, dt / 0.32) };
        self.pop_s = if snap { 1.0 } else { logic::toward(self.pop_s, 1.0, dt / 0.13) };
        self.pop_a = if snap { 1.0 } else { logic::toward(self.pop_a, 1.0, dt / 0.13) };
        self.sw = if snap { 1.0 } else { logic::toward(self.sw, 1.0, dt / 0.16) };
        if let Some(sw) = self.switch.as_mut() {
            sw.t = if snap { 1.0 } else { (sw.t + dt / SWITCH_OUT).min(1.0) };
        }
        let kt = self.kw_target();
        self.kw = if snap { kt } else { logic::follow(self.kw, kt, dt, 24.0, 0.004) };
        let rt = self.rw_target();
        self.rw = if snap { rt } else { logic::follow(self.rw, rt, dt, 22.0, 0.004) };
        let wt = self.wn_target();
        self.wn_x = if snap { wt } else { logic::follow(self.wn_x, wt, dt, 16.0, 0.004) };
        let (gc, gr) = self.gauge_goal();
        self.cpu_s = if snap { gc } else { logic::follow(self.cpu_s, gc, dt, 8.0, 0.002) };
        self.ram_s = if snap { gr } else { logic::follow(self.ram_s, gr, dt, 8.0, 0.002) };
        let at = if self.armed.is_some() { 1.0 } else { 0.0 };
        self.arm_t = if snap { at } else { logic::follow(self.arm_t, at, dt, 16.0, 0.01) };
        self.ai.glide(dt, snap);
        let ft = if self.focus == Focus::Action { 1.0 } else { 0.0 };
        self.focus_t = if snap { ft } else { logic::follow(self.focus_t, ft, dt, 18.0, 0.01) };
        let st = self.sel_target();
        // a list that is just opening, or a jump over many rows, does not slide
        self.sel_y = if snap || self.lh < 1.0 || (st - self.sel_y).abs() > 7.0 {
            st
        } else {
            logic::follow(self.sel_y, st, dt, 24.0, 0.01)
        };
        self.needs_anim()
    }

    /// The window goes in the upper part of the screen the pointer is on.
    fn place(&mut self, ui_scale: f32) -> bool {
        unsafe {
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
            if !GetMonitorInfoW(mon, &mut mi).as_bool() {
                return false;
            }
            let (mut dx, mut dy) = (96u32, 96u32);
            let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
            let scale = dx as f32 / 96.0 * ui_scale;
            let work = mi.rcWork;
            let (wpx, hpx) = ((W3 * scale).ceil() as i32, (FULL_H * scale).ceil() as i32);
            let x = (work.left + work.right) / 2 - wpx / 2;
            // the two boxes sit in the middle of the screen (a bit above the exact middle); the list drops below them
            let mid = work.top + ((work.bottom - work.top) as f32 * 0.40) as i32;
            let mut y = mid - (FRAME_H * scale * 0.5) as i32;
            if y + hpx > work.bottom - 8 {
                y = (work.bottom - 8 - hpx).max(work.top + 8);
            }
            let geom = (x, y, wpx, hpx);
            if geom != self.win_px || (scale - self.scale).abs() > 0.001 {
                self.scale = scale;
                self.win_px = geom;
                let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), x, y, wpx, hpx, SWP_NOACTIVATE);
                let _ = self.host.resize(wpx, hpx);
                self.region_h = -1;
            }
        }
        true
    }

    /// Only the glass takes clicks (the boxes and, while it shows, the list below them); the rest of the
    /// window is empty. `list_h` is how tall the floating list is (0 = none).
    fn set_region(&mut self, list_h: f32, wide: f32, burst: bool) {
        let (_, _, wpx, hpx) = self.win_px;
        if burst {
            // while the card flies in or out it reaches past its resting shape: let the whole window show
            if self.region_h == -2 || wpx == 0 {
                return;
            }
            self.region_h = -2;
            self.region_x = -2;
            unsafe {
                let rgn = CreateRectRgn(0, 0, wpx, hpx);
                SetWindowRgn(self.hwnd, Some(rgn), false);
            }
            return;
        }
        let bottom = if list_h > 0.5 { LIST_Y + list_h } else { FRAME_H };
        let h = ((bottom * self.scale).ceil() as i32).min(hpx);
        // the card sits in the middle of the window; with three boxes it reaches both edges
        let grown = E * wide.clamp(0.0, 1.0);
        let l = (((E - grown) * self.scale).floor() as i32).max(0);
        let r = (((E + W + grown) * self.scale).ceil() as i32).min(wpx);
        if (h == self.region_h && l == self.region_x) || wpx == 0 {
            return;
        }
        self.region_h = h;
        self.region_x = l;
        unsafe {
            let rgn = CreateRectRgn(l, 0, r, h);
            SetWindowRgn(self.hwnd, Some(rgn), false);
        }
    }

    // -- search -----------------------------------------------------------------------------------------

    fn search(&mut self) {
        self.search_inner();
        self.armed = None;
        // choosing where an action goes: only apps (open with) or folders (move, copy) are offered
        if let Some((a, _)) = &self.pending {
            let apps = *a == Act::OpenWith;
            self.hits.retain(|h| {
                if apps {
                    h.kind == Kind::App && !h.path.starts_with("shell:")
                } else {
                    h.kind == Kind::Folder
                }
            });
            if self.hits.is_empty() && !apps && self.query.trim().is_empty() {
                let folders: Vec<Hit> = self
                    .files
                    .iter()
                    .filter(|e| e.kind == Kind::Folder)
                    .take(12)
                    .map(|e| Hit {
                        name: e.name().to_string(),
                        detail: e.path.to_string(),
                        path: e.path.to_string(),
                        kind: Kind::Folder,
                        spans: Vec::new(),
                        idx: 0,
                    })
                    .collect();
                self.hits = folders;
            }
        }
    }

    fn search_inner(&mut self) {
        if self.mode == Mode::Windows {
            self.hits.clear();
            self.wx = None;
            return;
        }
        if self.mode == Mode::Run {
            self.hits.clear();
            self.wx = None;
            let q = self.query.trim().to_lowercase();
            for c in self.run_hist.iter().filter(|c| q.is_empty() || c.to_lowercase().contains(&q)) {
                self.hits.push(Hit {
                    name: c.clone(),
                    detail: "Recent command".into(),
                    path: c.clone(),
                    kind: Kind::Run,
                    spans: Vec::new(),
                    idx: 0,
                });
            }
            return;
        }
        if matches!(self.mode, Mode::Ai | Mode::Notes) {
            self.hits.clear();
            self.wx = None;
            return;
        }
        if self.mode == Mode::Emoji {
            self.hits.clear();
            self.wx = None;
            for i in emoji::search(&self.query) {
                let (ch, keys) = emoji::LIST[i];
                let name: Vec<&str> = keys.split_whitespace().take(2).collect();
                self.hits.push(Hit {
                    name: name.join(" "),
                    detail: "Emoji".into(),
                    path: ch.to_string(),
                    kind: Kind::Emoji,
                    spans: Vec::new(),
                    idx: i,
                });
            }
            return;
        }
        if self.mode == Mode::Kill {
            self.kill_search();
            return;
        }
        if self.mode == Mode::Clip {
            self.clip_search();
            return;
        }
        self.hits.clear();
        self.wx = None;
        let q = self.query.trim().to_string();
        if q.is_empty() {
            self.recent();
            return;
        }
        if logic::weather_query(&q) {
            self.wx = crate::modules::weather::brief();
            let search = self.cfg.search_url.clone();
            match self.wx.clone() {
                Some(b) => {
                    let url = format!("{search}{}", logic::enc(&format!("weather {}", b.place)));
                    self.hits.push(Hit {
                        name: format!("{:.0}{}", b.temp, b.unit()),
                        detail: format!("{} · {}", b.label, b.place),
                        path: url.clone(),
                        kind: Kind::Weather,
                        spans: Vec::new(),
                        idx: 0,
                    });
                    for (i, d) in b.days.iter().take(5).enumerate() {
                        let rain = d.rain.map(|r| format!("  ·  rain {r:.0}%")).unwrap_or_default();
                        self.hits.push(Hit {
                            name: d.label.clone(),
                            detail: format!("{:.0}° / {:.0}°{rain}", d.hi, d.lo),
                            path: url.clone(),
                            kind: Kind::Day,
                            spans: Vec::new(),
                            idx: i,
                        });
                    }
                }
                None => self.hits.push(Hit {
                    name: "--".into(),
                    detail: "No weather yet: set your city in Settings".into(),
                    path: format!("{search}{}", logic::enc("weather")),
                    kind: Kind::Weather,
                    spans: Vec::new(),
                    idx: 0,
                }),
            }
        }
        if let Some(h) = self.convert_hit(&q) {
            self.hits.push(h);
        } else if let Some(r) = logic::calc(&q) {
            self.hits.push(Hit {
                name: r,
                detail: q.clone(),
                path: String::new(),
                kind: Kind::Calc,
                spans: Vec::new(),
                idx: 0,
            });
        } else if let Some(u) = logic::as_url(&q) {
            self.hits.push(Hit {
                name: u.clone(),
                detail: "Open in the browser".into(),
                path: u,
                kind: Kind::Url,
                spans: Vec::new(),
                idx: 0,
            });
        }
        // "restart", "shut down", "lock" ...
        for i in conv::system(&q) {
            self.hits.push(Hit {
                name: conv::SYSTEM[i].0.to_string(),
                detail: "System".into(),
                path: format!("sys:{i}"),
                kind: Kind::System,
                spans: Vec::new(),
                idx: i,
            });
        }
        let ql = q.to_ascii_lowercase();
        let qb = &ql.as_bytes()[..ql.len().min(40)];
        let na = self.apps.len();
        let mut pos = std::mem::take(&mut self.scratch);
        let mut best: Vec<(i32, u32)> = Vec::with_capacity(KEEP + 1);
        // what was chosen before for this typing (or for a longer typing that starts like it) comes first
        let mut learned: HashMap<&str, i32> = HashMap::new();
        if ql.len() >= 2 {
            for ((typed, path), &n) in &self.learn {
                if typed.starts_with(&ql) {
                    let n = n.min(4) as i32;
                    let b = if typed.len() == ql.len() { 4_000 + 1_000 * n } else { 2_000 + 500 * n };
                    let e = learned.entry(path.as_str()).or_insert(0);
                    *e = (*e).max(b);
                }
            }
        }
        for (k, e) in self.apps.iter().chain(self.files.iter()).enumerate() {
            let name = e.name();
            let Some(s) = logic::matched(name.as_bytes(), qb, k < na || qb.len() >= 3, &mut pos) else { continue };
            let mut s = s - name.len().min(80) as i32;
            if !learned.is_empty() {
                s += learned.get(&*e.path).copied().unwrap_or(0);
            }
            s += match e.kind {
                Kind::App => 800,
                Kind::Folder => 100,
                _ => 0,
            };
            if let Some(&n) = self.usage.get(&*e.path) {
                s += n.min(20) as i32 * 12;
            }
            if best.len() < KEEP || s > best[best.len() - 1].0 {
                let at = best.partition_point(|b| b.0 >= s);
                best.insert(at, (s, k as u32));
                best.truncate(KEEP);
            }
        }
        for &(_, k) in &best {
            let k = k as usize;
            let e = if k < na { &self.apps[k] } else { &self.files[k - na] };
            let _ = logic::matched(e.name().as_bytes(), qb, k < na || qb.len() >= 3, &mut pos);
            let detail = match e.kind {
                Kind::App if e.path.starts_with("shell:") => "Store app".to_string(),
                Kind::App => "Application".to_string(),
                _ => e.path.to_string(),
            };
            self.hits.push(Hit {
                name: e.name().to_string(),
                detail,
                path: e.path.to_string(),
                kind: e.kind,
                spans: logic::spans(&pos),
                idx: 0,
            });
        }
        self.scratch = pos;
        let url = format!("{}{}", self.cfg.search_url, logic::enc(&q));
        self.hits.push(Hit {
            name: format!("Search the web for “{}”", logic::mid(&q, 40)),
            detail: "Web search".into(),
            path: url,
            kind: Kind::Web,
            spans: Vec::new(),
            idx: 0,
        });
    }

    /// The Kill mode looks at the programs only while it is showing.
    fn sync_procs(&mut self) {
        if self.mode == Mode::Kill {
            if self.sampler.is_none() {
                if let Ok(mut g) = self.procs_slot.lock() {
                    *g = None;
                }
                let (s, first) = procs::start(self.procs_slot.clone(), SendHwnd::new(self.hwnd), WM_PROCS);
                self.sampler = Some(s);
                self.snap = Some(first);
            }
        } else if self.sampler.take().is_some() {
            self.snap = None;
        }
    }

    /// The programs that fit what was typed (with nothing typed: the ones using the most memory).
    fn kill_search(&mut self) {
        self.hits.clear();
        self.wx = None;
        let Some(snap) = self.snap.as_ref() else { return };
        for i in procs::rank(&snap.procs, &self.query) {
            let p = &snap.procs[i];
            self.hits.push(Hit {
                name: procs::title(&p.name),
                detail: if p.locked {
                    format!("{} \u{b7} {} CPU \u{b7} protected", procs::mem_text(p.mem), procs::cpu_text(p.cpu))
                } else {
                    format!("{} \u{b7} {} CPU", procs::mem_text(p.mem), procs::cpu_text(p.cpu))
                },
                path: if p.path.is_empty() { p.name.clone() } else { p.path.clone() },
                kind: Kind::Process,
                spans: Vec::new(),
                idx: p.count as usize,
            });
        }
    }

    /// New numbers have arrived: the programs are listed again, and the selection stays on its program.
    fn refresh_procs(&mut self) {
        let keep = self.hits.get(self.sel).map(|h| h.path.clone());
        self.kill_search();
        if let Some(k) = keep {
            if let Some(i) = self.hits.iter().position(|h| h.path == k) {
                self.sel = i;
            }
        }
        self.sel = self.sel.min(self.hits.len().saturating_sub(1));
        self.top = self.top.min(self.hits.len().saturating_sub(VIS));
        if self.hits.is_empty() {
            self.list_open = false;
        }
        let gone = self.armed.as_ref().is_some_and(|a| !self.hits.iter().any(|h| &h.path == a));
        if gone {
            self.armed = None;
        }
    }

    /// Enter in the Kill mode: the first press asks, the second ends the program.
    fn kill_enter(&mut self) {
        let Some(h) = self.cur().cloned() else { return };
        if h.kind != Kind::Process {
            return;
        }
        let exe = h.path.rsplit(['\\', '/']).next().unwrap_or("").to_string();
        if exe.is_empty() || procs::protected(&exe) {
            self.armed = None;
            return;
        }
        if self.armed.as_deref() != Some(h.path.as_str()) {
            self.armed = Some(h.path.clone());
            self.pop_a = 0.0;
            self.kick();
            return;
        }
        self.armed = None;
        let name = exe.clone();
        let _ = std::thread::Builder::new().name("launcher-kill".into()).spawn(move || {
            taskkill(&name, true);
        });
        // it is gone from the boxes and the list at once
        if let Some(s) = self.snap.as_mut() {
            s.procs.retain(|p| !p.name.eq_ignore_ascii_case(&exe));
        }
        let at = self.sel;
        self.kill_search();
        self.sel = at.min(self.hits.len().saturating_sub(1));
        self.top = self.top.min(self.hits.len().saturating_sub(VIS));
        if self.hits.is_empty() {
            self.list_open = false;
        }
        self.ul = 0.0;
        self.pop_s = 0.0;
        self.pop_a = 0.0;
        self.kick();
    }

    /// Clipboard mode: what was copied (the letters typed narrow it down).
    fn clip_search(&mut self) {
        self.hits.clear();
        self.wx = None;
        for r in crate::cliphist::rows(&self.query) {
            let path = format!("clip:{}", r.id);
            if let Some(img) = r.thumb {
                self.icons.insert(path.clone(), Some(img));
            }
            let kind = ["Text", "Link", "Image", "Files"][usize::from(r.code).min(3)];
            self.hits.push(Hit {
                name: r.summary,
                detail: format!("{kind} · {}", logic::ago(r.age_secs)),
                path,
                kind: Kind::Clip,
                spans: Vec::new(),
                idx: usize::from(r.code),
            });
        }
    }

    /// Nothing typed yet: the apps used most.
    fn recent(&mut self) {
        let mut used: Vec<(&String, &u32)> = self.usage.iter().collect();
        used.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        for (path, _) in used.into_iter().take(8) {
            if let Some(e) = self.apps.iter().find(|e| &*e.path == path.as_str()) {
                let detail = if e.path.starts_with("shell:") { "Store app" } else { "Application" };
                self.hits.push(Hit {
                    name: e.name().to_string(),
                    detail: detail.into(),
                    path: e.path.to_string(),
                    kind: Kind::App,
                    spans: Vec::new(),
                    idx: 0,
                });
            }
        }
    }

    /// The text changed: start again from the best match.
    fn changed(&mut self) {
        self.run_err = false;
        let before = self.cur().map(|h| (h.path.clone(), h.kind));
        self.focus = Focus::Subject;
        self.sel = 0;
        self.top = 0;
        self.act = 0;
        self.search();
        if self.cur().map(|h| (h.path.clone(), h.kind)) != before {
            // a different best match: everything about it comes in again
            self.ul = 0.0;
            self.pop_s = 0.0;
            self.pop_a = 0.0;
        } else {
            // the same one with more letters underlined: only the underline grows
            self.ul = self.ul.min(0.5);
        }
        if self.hits.is_empty() {
            self.list_open = false;
        }
    }

    fn shows_hit(&self) -> bool {
        !self.hits.is_empty() && (self.one_box || !self.query.trim().is_empty() || self.list_open)
    }

    fn cur(&self) -> Option<&Hit> {
        if self.shows_hit() {
            self.hits.get(self.sel)
        } else {
            None
        }
    }

    fn rows_len(&self) -> usize {
        match self.focus {
            Focus::Subject => self.hits.len(),
            Focus::Action => self.hits.get(self.sel).map_or(0, |h| actions(h).len()),
        }
    }

    fn cursor(&self) -> usize {
        match self.focus {
            Focus::Subject => self.sel,
            Focus::Action => self.act,
        }
    }

    fn set_cursor(&mut self, i: usize) {
        match self.focus {
            Focus::Subject => {
                if i != self.sel {
                    self.sel = i;
                    self.armed = None;
                    self.act = 0;
                    self.ul = 0.0;
                    self.pop_s = 0.0;
                    self.pop_a = 0.0;
                }
                if i < self.top {
                    self.top = i;
                } else if i >= self.top + VIS {
                    self.top = i + 1 - VIS;
                }
            }
            Focus::Action => {
                if i != self.act {
                    self.act = i;
                    self.armed = None;
                    self.pop_a = 0.0;
                }
            }
        }
    }

    fn step(&mut self, d: i32) {
        let len = self.rows_len();
        if len == 0 {
            return;
        }
        let n = (self.cursor() as i32 + d).clamp(0, len as i32 - 1) as usize;
        self.set_cursor(n);
    }

    fn view_top(&self) -> usize {
        if self.focus == Focus::Subject {
            self.top
        } else {
            0
        }
    }

    // -- doing it ----------------------------------------------------------------------------------------

    /// "5 km in miles", "20 usd in eur", "time in tokyo": the answer as a calculator hit.
    fn convert_hit(&mut self, q: &str) -> Option<Hit> {
        let (name, detail) = match conv::convert(q) {
            Some(conv::Conv::Done { result, detail }) => (result, detail),
            Some(conv::Conv::Money { amount, from, to }) => match rate(&from, &to) {
                Some(r) => match conv::money(amount, &from, &to, r) {
                    conv::Conv::Done { result, detail } => (result, detail),
                    _ => return None,
                },
                None => {
                    start_rates(SendHwnd::new(self.hwnd));
                    (
                        "\u{2026}".to_string(),
                        format!("{} {from} \u{2192} {to} \u{b7} getting today's rates", conv::fmt(amount)),
                    )
                }
            },
            None => conv::time(q, unix_secs() as i64)?,
        };
        Some(Hit { name, detail, path: String::new(), kind: Kind::Calc, spans: Vec::new(), idx: 0 })
    }

    /// Save the note that was typed (Enter in the notes mode).
    fn save_note(&mut self) {
        let line: String = self.query.trim().chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
        if line.is_empty() {
            return;
        }
        let at = unix_secs();
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(notes_path()) {
            use std::io::Write;
            let _ = writeln!(f, "{at}\t{line}");
        }
        self.notes.insert(0, (at, line));
        self.notes.truncate(12);
        self.notes_text = notes_text(&self.notes);
        self.query.clear();
        self.ai.q = Glide::new(Q_SIZES[0]);
        self.ai.scroll = 0.0;
    }

    /// Open the file with the chosen app, or move or copy it into the chosen folder.
    fn run_target(&mut self, act: Act, subject: Hit, target: Hit) {
        self.hide();
        let (src, dst) = (subject.path, target.path);
        std::thread::spawn(move || match act {
            Act::OpenWith => {
                let exe = if dst.to_ascii_lowercase().ends_with(".exe") {
                    Some(dst.clone())
                } else {
                    crate::dock::lnk_target(&dst)
                };
                match exe {
                    Some(exe) => {
                        let _ = std::process::Command::new(exe).arg(&src).spawn();
                    }
                    None => sys::shell_open(&src),
                }
            }
            Act::MoveTo | Act::CopyTo => {
                let from = Path::new(&src);
                let Some(name) = from.file_name() else { return };
                let to = Path::new(&dst).join(name);
                if to.exists() {
                    return;
                }
                if act == Act::MoveTo && std::fs::rename(from, &to).is_ok() {
                    return;
                }
                if copy_all(from, &to).is_ok() && act == Act::MoveTo {
                    let _ = if from.is_dir() { std::fs::remove_dir_all(from) } else { std::fs::remove_file(from) };
                }
            }
            _ => {}
        });
    }

    /// The picture or the first lines of the file on the left, loaded the first time it is shown.
    fn load_peek(&mut self, path: &str) {
        let ext = Path::new(path).extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        const IMAGES: [&str; 9] = ["png", "jpg", "jpeg", "gif", "bmp", "webp", "tif", "tiff", "ico"];
        const TEXTS: [&str; 22] = [
            "txt", "md", "log", "json", "toml", "yaml", "yml", "csv", "rs", "py", "js", "ts", "html", "css", "xml",
            "ini", "cfg", "c", "h", "cpp", "bat", "ps1",
        ];
        if IMAGES.contains(&ext.as_str()) {
            let key = format!("prev:{path}");
            if !self.icons.contains_key(&key) {
                let img = dragdrop::thumbnail(path, 256);
                self.icons.insert(key, img);
            }
        } else if TEXTS.contains(&ext.as_str()) && !self.texts.contains_key(path) {
            use std::io::Read;
            if self.texts.len() > 16 {
                self.texts.clear();
            }
            let mut buf = vec![0u8; 1500];
            let n = std::fs::File::open(path).and_then(|mut f| f.read(&mut buf)).unwrap_or(0);
            buf.truncate(n);
            let text = if buf.contains(&0) { String::new() } else { String::from_utf8_lossy(&buf).into_owned() };
            self.texts.insert(path.to_string(), text);
        }
    }

    /// Send the question to Gemini (the key is the one from Settings → Gemini).
    fn ask(&mut self) {
        let question = self.query.trim().to_string();
        if question.is_empty() {
            return;
        }
        self.ai.clear();
        let key = crate::sys::dotenv::get_saved_first(crate::modules::gemini::KEY_VAR)
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty());
        let Some(key) = key else {
            self.ai.state = AiState::Failed;
            self.ai.text = "There is no Gemini key yet. Paste one in Settings \u{2192} Gemini and ask again.".into();
            return;
        };
        self.ai.state = AiState::Waiting;
        self.ai.cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        if let Ok(mut sh) = self.ai.shared.lock() {
            *sh = AiShared { seq: self.ai.seq, text: String::new(), done: false, err: None };
        }
        spawn_ai(
            self.ai.shared.clone(),
            SendHwnd::new(self.hwnd),
            self.ai.seq,
            self.ai.cancel.clone(),
            self.cfg.gemini.clone(),
            key,
            question,
        );
    }

    /// Take what the request has written so far. True when the picture needs to change.
    fn ai_poll(&mut self) -> bool {
        let (text, done, err) = match self.ai.shared.lock() {
            Ok(sh) if sh.seq == self.ai.seq && self.ai.state == AiState::Waiting => {
                (sh.text.clone(), sh.done, sh.err.clone())
            }
            _ => return false,
        };
        let before = (self.ai.text.len(), self.ai.state);
        if let Some(e) = err {
            self.ai.state = AiState::Failed;
            self.ai.text = if text.is_empty() { e } else { format!("{text}\n\n{e}") };
        } else {
            self.ai.text = text;
            if done {
                self.ai.state = AiState::Done;
            }
        }
        before != (self.ai.text.len(), self.ai.state)
    }

    /// Keys that mean something else in Gemini mode. True when the key was used.
    fn ai_key(&mut self, vk: u16, ctrl: bool) -> bool {
        let step = |me: &mut Self, d: f32| me.ai.scroll = (me.ai.scroll + d).clamp(0.0, me.ai.max_scroll);
        match VIRTUAL_KEY(vk) {
            VK_RETURN if self.mode == Mode::Notes => self.save_note(),
            VK_RETURN => self.ask(),
            VK_DOWN => step(self, 24.0),
            VK_UP => step(self, -24.0),
            VK_NEXT => step(self, 120.0),
            VK_PRIOR => step(self, -120.0),
            VK_HOME => step(self, f32::MIN / 2.0),
            VK_END => step(self, f32::MAX / 2.0),
            // Ctrl+C: take the answer along
            VK_C if ctrl && self.ai.state == AiState::Done => {
                clip::set_text(self.hwnd, &self.ai.text);
            }
            _ => return false,
        }
        true
    }

    /// Enter in the Run box: start the highlighted recent command, or else what was typed, the way the Windows
    /// Run box does, and remember it. When it cannot be started the box stays open and says so.
    fn run_cmd(&mut self) {
        let picked = if self.list_open { self.hits.get(self.sel).map(|h| h.path.clone()) } else { None };
        let cmd = picked.unwrap_or_else(|| self.query.trim().to_string());
        if cmd.is_empty() {
            return;
        }
        if !sys::shell_run(&cmd) {
            self.run_err = true;
            return;
        }
        self.run_hist.retain(|c| !c.eq_ignore_ascii_case(&cmd));
        self.run_hist.insert(0, cmd);
        self.run_hist.truncate(RUN_KEEP);
        save_run_history(&self.run_hist);
        self.hide();
    }

    fn query_limit(&self) -> usize {
        if matches!(self.mode, Mode::Ai | Mode::Notes) {
            400
        } else {
            200
        }
    }

    fn run(&mut self) {
        if self.mode == Mode::Run {
            self.run_cmd();
            return;
        }
        let Some(h) = self.cur().cloned() else { return };
        let acts = actions(&h);
        let a = acts[self.act.min(acts.len() - 1)];
        // choosing where an action goes: Enter finishes it
        if let Some((pa, subject)) = self.pending.take() {
            self.run_target(pa, subject, h);
            return;
        }
        if matches!(a, Act::OpenWith | Act::MoveTo | Act::CopyTo) {
            self.pending = Some((a, h));
            self.query.clear();
            self.sel = 0;
            self.top = 0;
            self.act = 0;
            self.focus = Focus::Subject;
            self.list_open = false;
            self.search();
            self.ul = 0.0;
            self.pop_s = 0.0;
            self.pop_a = 0.0;
            self.kick();
            return;
        }
        if a == Act::Uninstall {
            // taking an app off the computer asks twice
            if self.armed.as_deref() != Some(h.path.as_str()) {
                self.armed = Some(h.path.clone());
                return;
            }
            self.hide();
            // it leaves the list now; the apps are looked for again once the uninstaller has had time
            self.apps.retain(|e| &*e.path != h.path.as_str());
            unsafe {
                SetTimer(Some(self.hwnd), T_RESCAN, 20_000, None);
            }
            uninstall_app(&h.path, &h.name);
            return;
        }
        if a == Act::Run && h.kind == Kind::System && h.idx == conv::PICK_COLOR {
            self.start_pick();
            return;
        }
        if a == Act::Run {
            // putting the computer off asks twice
            if conv::SYSTEM.get(h.idx).is_some_and(|c| c.2) && self.armed.as_deref() != Some(h.path.as_str()) {
                self.armed = Some(h.path.clone());
                return;
            }
            self.hide();
            run_system(h.idx);
            return;
        }
        let hwnd = self.hwnd;
        let typed = self.query.trim().to_ascii_lowercase();
        if a == Act::DeleteClip {
            // stays open: the item just leaves the list
            if let Some(id) = clip_id(&h.path) {
                crate::cliphist::remove(id);
            }
            let keep = self.sel;
            self.focus = Focus::Subject;
            self.act = 0;
            self.search();
            self.sel = keep.min(self.hits.len().saturating_sub(1));
            self.kick();
            return;
        }
        self.hide();
        match a {
            Act::Paste | Act::CopyClip => {
                if let Some(id) = clip_id(&h.path) {
                    if crate::cliphist::copy_back(hwnd, id) && a == Act::Paste {
                        paste_soon();
                    }
                }
            }
            Act::DeleteClip | Act::OpenWith | Act::MoveTo | Act::CopyTo | Act::Run | Act::Uninstall | Act::Kill => {}
            Act::PasteEmoji | Act::CopyEmoji => {
                clip::set_text(hwnd, &h.path);
                if a == Act::PasteEmoji {
                    paste_soon();
                }
            }
            Act::Open | Act::OpenLink | Act::Search | Act::Forecast => sys::shell_open(&h.path),
            Act::Admin => run_as_admin(&h.path),
            Act::Reveal => sys::shell_reveal(&h.path),
            Act::Quit | Act::ForceQuit => {
                if let Some(exe) = exe_name(&h.path) {
                    taskkill(&exe, a == Act::ForceQuit);
                }
            }
            Act::Restart => {
                if let Some(exe) = exe_name(&h.path) {
                    restart_app(exe, h.path.clone());
                }
            }
            Act::CopyPath => {
                clip::set_text(hwnd, &h.path);
            }
            Act::CopyResult => {
                clip::set_text(hwnd, &h.name);
            }
        }
        if h.kind.has_icon() && matches!(a, Act::Open | Act::Admin) {
            self.bump(&h.path);
            self.teach(&typed, &h.path);
        }
    }

    /// Remember what was chosen for what was typed: the same typing finds it first next time. Other
    /// things chosen for the same typing before count a little less after this.
    fn teach(&mut self, typed: &str, path: &str) {
        if typed.len() < 2 || typed.len() > 40 {
            return;
        }
        let typed = typed.to_string();
        self.learn.retain(|(t, p), n| {
            if *t == typed && p != path {
                *n = n.saturating_sub(1);
            }
            *n > 0
        });
        *self.learn.entry((typed, path.to_string())).or_insert(0) += 1;
        if self.learn.len() > 400 {
            if let Some(low) = self.learn.iter().min_by_key(|(_, n)| **n).map(|(k, _)| k.clone()) {
                self.learn.remove(&low);
            }
        }
        save_learn(&self.learn);
    }

    /// Remember what was opened: it ranks higher next time.
    fn bump(&mut self, path: &str) {
        *self.usage.entry(path.to_string()).or_insert(0) += 1;
        if self.usage.len() > 400 {
            if let Some(low) = self.usage.iter().min_by_key(|(_, n)| **n).map(|(k, _)| k.clone()) {
                self.usage.remove(&low);
            }
        }
        save_usage(&self.usage);
    }

    // -- input -------------------------------------------------------------------------------------------

    fn key(&mut self, gfx: &Gfx, vk: u16) {
        let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
        let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
        let has_rows = !self.hits.is_empty();
        if self.pick.is_some() {
            self.pick_key(gfx, vk);
            return;
        }
        if self.mode == Mode::Windows {
            self.windows_key(gfx, vk, shift);
            return;
        }
        if VIRTUAL_KEY(vk) == VK_TAB {
            // the Run box is only reached with its own hotkey
            if self.mode == Mode::Run {
                return;
            }
            // Search -> clipboard -> Gemini (Shift+Tab goes back); Left and Right move between the two boxes
            if !self.cfg.animate {
                self.mode = self.mode.cycle(shift);
                self.sw = 1.0;
                self.show(gfx, 1.0, true);
                return;
            }
            // the old contents shrink away first; pressing Tab again meanwhile just aims further along
            let to = self.switch.as_ref().map_or(self.mode, |sw| sw.to).cycle(shift);
            match self.switch.as_mut() {
                Some(sw) => sw.to = to,
                None => self.switch = Some(Switch { to, t: 0.0 }),
            }
            self.kick();
            return;
        }
        if matches!(self.mode, Mode::Ai | Mode::Notes) && self.ai_key(vk, ctrl) {
            self.draw(gfx);
            return;
        }
        if self.mode == Mode::Run {
            match VIRTUAL_KEY(vk) {
                VK_RETURN => {
                    self.run_cmd();
                    self.draw(gfx);
                    return;
                }
                // Right takes the highlighted command into the box, to change it before it runs
                VK_RIGHT if self.list_open => {
                    if let Some(c) = self.hits.get(self.sel).map(|h| h.path.clone()) {
                        self.query = c;
                        self.list_open = false;
                        self.changed();
                    }
                    self.draw(gfx);
                    return;
                }
                VK_LEFT | VK_RIGHT => return,
                // Delete forgets the highlighted command
                VK_DELETE if self.list_open && !self.hits.is_empty() => {
                    if let Some(c) = self.hits.get(self.sel).map(|h| h.path.clone()) {
                        self.run_hist.retain(|x| *x != c);
                        save_run_history(&self.run_hist);
                    }
                    let keep = self.sel;
                    self.search();
                    self.sel = keep.min(self.hits.len().saturating_sub(1));
                    self.list_open = !self.hits.is_empty();
                    self.draw(gfx);
                    return;
                }
                _ => {}
            }
        }
        if self.mode == Mode::Kill {
            match VIRTUAL_KEY(vk) {
                VK_RETURN => {
                    self.kill_enter();
                    self.draw(gfx);
                    return;
                }
                // between the two programs the boxes show
                VK_LEFT | VK_RIGHT => {
                    if !self.hits.is_empty() && !self.query.trim().is_empty() && !self.list_open {
                        let page = self.sel & !1;
                        let to =
                            if VIRTUAL_KEY(vk) == VK_RIGHT { (page + 1).min(self.hits.len() - 1) } else { page };
                        self.set_cursor(to);
                        self.draw(gfx);
                    }
                    return;
                }
                _ => {}
            }
        }
        match VIRTUAL_KEY(vk) {
            VK_ESCAPE => {
                if self.pending.is_some() {
                    // back from choosing where it goes
                    self.pending = None;
                    self.query.clear();
                    self.search();
                    self.ul = 0.0;
                    self.pop_s = 0.0;
                    self.pop_a = 0.0;
                } else if self.list_open && matches!(self.mode, Mode::Search | Mode::Kill | Mode::Run) {
                    self.list_open = false;
                } else {
                    self.hide();
                    return;
                }
            }
            VK_RETURN => {
                self.run();
                self.draw(gfx);
                return;
            }
            // Ctrl+K: the menu of things to do with the match (quit, restart, force quit, ...)
            VIRTUAL_KEY(0x4B) if ctrl => {
                if self.focus == Focus::Action && self.list_open {
                    self.focus = Focus::Subject;
                    self.list_open = false;
                } else if self.cur().is_some_and(|h| actions(h).len() > 1) {
                    self.focus = Focus::Action;
                    self.list_open = true;
                    self.act = 0;
                    self.pop_a = 0.0;
                    let c = self.cursor();
                    self.set_cursor(c);
                }
            }
            VK_RIGHT => {
                if self.cur().is_some() {
                    self.focus = Focus::Action;
                }
            }
            VK_LEFT => self.focus = Focus::Subject,
            VK_DOWN => {
                if !self.list_open {
                    if has_rows && self.rows_len() > 0 {
                        self.list_open = true;
                        let c = self.cursor();
                        self.set_cursor(c);
                    }
                } else {
                    self.step(1);
                }
            }
            VK_UP => {
                if self.list_open {
                    if self.cursor() == 0 && matches!(self.mode, Mode::Search | Mode::Kill | Mode::Run) {
                        self.list_open = false;
                    } else {
                        self.step(-1);
                    }
                }
            }
            VK_NEXT => {
                if self.list_open {
                    self.step(VIS as i32);
                }
            }
            VK_PRIOR => {
                if self.list_open {
                    self.step(-(VIS as i32));
                }
            }
            VK_HOME => {
                if self.list_open {
                    self.step(i32::MIN / 2);
                }
            }
            VK_END => {
                if self.list_open {
                    self.step(i32::MAX / 2);
                }
            }
            VK_BACK => {
                if ctrl {
                    while self.query.ends_with(' ') {
                        self.query.pop();
                    }
                    while self.query.chars().last().is_some_and(|c| c != ' ') {
                        self.query.pop();
                    }
                } else {
                    self.query.pop();
                }
                self.changed();
            }
            // clipboard mode: Delete takes the item out of the history
            VK_DELETE if self.mode == Mode::Clip && has_rows => {
                if let Some(id) = self.cur().and_then(|h| clip_id(&h.path)) {
                    crate::cliphist::remove(id);
                }
                let keep = self.sel;
                self.search();
                self.sel = keep.min(self.hits.len().saturating_sub(1));
            }
            VK_DELETE => {
                self.query.clear();
                self.changed();
            }
            VK_V if ctrl => {
                if let Some(t) = clip::get_text(self.hwnd) {
                    let room = self.query_limit().saturating_sub(self.query.chars().count());
                    let one: String = if matches!(self.mode, Mode::Ai | Mode::Notes) {
                        // a question may run over several lines: they are joined
                        t.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(room).collect()
                    } else {
                        t.lines().next().unwrap_or("").chars().filter(|c| !c.is_control()).take(room).collect()
                    };
                    self.query.push_str(&one);
                    self.changed();
                }
            }
            _ => return,
        }
        self.draw(gfx);
    }

    /// The colour picker opens in place of the two boxes' contents: the pointer can go anywhere on the
    /// screen and the boxes show what is under it; Enter copies it.
    fn start_pick(&mut self) {
        self.pick = Some(Pick { cells: vec![0; (PICK_N * PICK_N) as usize], row: 0 });
        self.list_open = false;
        self.sample();
        self.sw = if self.cfg.animate { 0.0 } else { 1.0 };
        unsafe {
            SetTimer(Some(self.hwnd), T_PICK, 40, None);
        }
        self.kick();
    }

    /// Look for the installed apps again (in the background; the list is replaced when that is done).
    fn rescan_apps(&mut self) {
        self.apps_at = Instant::now();
        spawn_apps(self.apps_slot.clone(), SendHwnd::new(self.hwnd));
    }

    fn stop_pick(&mut self) {
        if self.pick.take().is_some() {
            unsafe {
                let _ = KillTimer(Some(self.hwnd), T_PICK);
            }
        }
    }

    /// Look at the pixels around the pointer again.
    fn sample(&mut self) {
        let Some(pk) = self.pick.as_mut() else { return };
        let mut pt = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut pt);
        }
        let half = PICK_N / 2;
        if let Some(shot) = crate::refract::grab(pt.x - half, pt.y - half, PICK_N, PICK_N) {
            if shot.px.len() == pk.cells.len() {
                pk.cells.copy_from_slice(&shot.px);
            }
        }
    }

    /// Keys while the colour picker is open: Up and Down choose the way of writing, Enter copies it, Esc goes back.
    fn pick_key(&mut self, gfx: &Gfx, vk: u16) {
        match VIRTUAL_KEY(vk) {
            VK_ESCAPE => self.stop_pick(),
            VK_UP | VK_DOWN => {
                if let Some(pk) = self.pick.as_mut() {
                    pk.row = if VIRTUAL_KEY(vk) == VK_UP { (pk.row + 2) % 3 } else { (pk.row + 1) % 3 };
                }
            }
            VK_RETURN => {
                if let Some(pk) = &self.pick {
                    let copy = pk.texts()[pk.row.min(2)].1.clone();
                    clip::set_text(self.hwnd, &copy);
                }
                self.stop_pick();
                self.hide();
                return;
            }
            _ => return,
        }
        self.draw(gfx);
    }

    fn typed(&mut self, gfx: &Gfx, c: char) {
        if self.pick.is_some()
            || self.mode == Mode::Windows
            || c.is_control()
            || (c == ' ' && self.query.is_empty())
            || self.query.chars().count() >= self.query_limit()
        {
            return;
        }
        self.query.push(c);
        self.changed();
        self.draw(gfx);
    }

    fn pt(&self, lp: LPARAM) -> (f32, f32) {
        let x = (lp.0 & 0xFFFF) as u16 as i16 as f32;
        let y = ((lp.0 >> 16) & 0xFFFF) as u16 as i16 as f32;
        // the card sits `E` in from the left edge of the window
        (x / self.scale - E, y / self.scale)
    }

    fn row_at(&self, x: f32, y: f32) -> Option<usize> {
        let list = self.list_open && self.rows_len() > 0;
        let y0 = LIST_Y + HEAD;
        if !list || x < 10.0 || x > W - 10.0 || y < y0 {
            return None;
        }
        let k = ((y - y0) / ROW) as usize;
        (k < self.rows_len().min(self.vis())).then(|| self.view_top() + k)
    }

    pub fn handle(&mut self, gfx: &Gfx, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_OUTSIDE => {
                // a click anywhere else puts it away (the colour picker lets the pointer go anywhere)
                if self.open && self.pick.is_none() {
                    self.hide();
                }
                Some(LRESULT(0))
            }
            WM_ACTIVATE => {
                // losing the focus alone no longer closes it (a pop-up or a notification may take it for a
                // moment); only a click does: this catches the click on a window the watcher cannot see
                if (wp.0 & 0xFFFF) == 0 && self.open && self.pick.is_none() && mouse_down() {
                    self.hide();
                }
                Some(LRESULT(0))
            }
            WM_ERASEBKGND => Some(LRESULT(1)),
            WM_KEYDOWN | WM_SYSKEYDOWN if self.open => {
                self.key(gfx, wp.0 as u16);
                Some(LRESULT(0))
            }
            WM_CHAR | WM_SYSCHAR if self.open => {
                let u = wp.0 as u16;
                let ch = if (0xD800..0xDC00).contains(&u) {
                    self.high = Some(u);
                    None
                } else if (0xDC00..0xE000).contains(&u) {
                    self.high.take().and_then(|hi| char::decode_utf16([hi, u]).next()?.ok())
                } else {
                    char::from_u32(u as u32)
                };
                if let Some(c) = ch {
                    self.typed(gfx, c);
                }
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE if self.open => {
                let (x, y) = self.pt(lp);
                if self.mouse.0 < 0.0 {
                    // the first report only says where the pointer already was
                    self.mouse = (x, y);
                    return Some(LRESULT(0));
                }
                if (x - self.mouse.0).abs() + (y - self.mouse.1).abs() < 1.0 {
                    return Some(LRESULT(0));
                }
                self.mouse = (x, y);
                if let Some(i) = self.row_at(x, y) {
                    if i != self.cursor() {
                        self.set_cursor(i);
                        self.draw(gfx);
                    }
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONUP if self.open && self.mode == Mode::Windows => {
                let (x, y) = self.pt(lp);
                if let Some(i) = self.window_at(x, y) {
                    self.wn_sel = i;
                    self.windows_commit();
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONUP if self.open && self.mode == Mode::Kill && self.pick.is_none() => {
                let (x, y) = self.pt(lp);
                let typing = !self.query.trim().is_empty();
                if let Some(i) = self.row_at(x, y) {
                    // a program of the list: it is chosen; Enter twice ends it
                    self.set_cursor(i);
                    if typing {
                        self.list_open = false;
                    }
                } else {
                    let bx = |i: f32| Rect::new(PAD - E + i * (BOX + GAP), PAD, BOX, BOX);
                    let page = self.sel & !1;
                    if bx(0.0).contains(x, y) {
                        self.kill_enter();
                    } else if typing && bx(1.0).contains(x, y) && page < self.hits.len() {
                        self.set_cursor(page);
                    } else if typing && bx(2.0).contains(x, y) && page + 1 < self.hits.len() {
                        self.set_cursor(page + 1);
                    }
                }
                self.draw(gfx);
                Some(LRESULT(0))
            }
            WM_LBUTTONUP if self.open => {
                let (x, y) = self.pt(lp);
                if let Some(i) = self.row_at(x, y) {
                    self.set_cursor(i);
                    self.run();
                    return Some(LRESULT(0));
                }
                let (sub, act) = if self.rw_target() > 0.5 {
                    (self.solo_rect(), Rect::new(0.0, 0.0, 0.0, 0.0))
                } else {
                    (Rect::new(PAD, PAD, BOX, BOX), Rect::new(PAD + BOX + GAP, PAD, BOX, BOX))
                };
                let want = if sub.contains(x, y) {
                    Some(Focus::Subject)
                } else if act.contains(x, y) {
                    Some(Focus::Action)
                } else {
                    None
                };
                if let Some(f) = want {
                    if f == Focus::Action && self.cur().is_none() {
                        return Some(LRESULT(0));
                    }
                    self.focus = f;
                    // clicking a box only picks it: the list opens with the Down arrow, never with a click
                    self.list_open = self.list_open && self.rows_len() > 0;
                    self.draw(gfx);
                }
                Some(LRESULT(0))
            }
            WM_MOUSEWHEEL if self.open && self.mode == Mode::Windows => {
                let d = ((wp.0 >> 16) & 0xFFFF) as u16 as i16 as i32 / 120;
                self.windows_move(gfx, if d > 0 { -1 } else { 1 });
                Some(LRESULT(0))
            }
            WM_MOUSEWHEEL if self.open => {
                if matches!(self.mode, Mode::Ai | Mode::Notes) {
                    let d = ((wp.0 >> 16) & 0xFFFF) as u16 as i16 as i32 / 120;
                    self.ai.scroll = (self.ai.scroll - d as f32 * 36.0).clamp(0.0, self.ai.max_scroll);
                    self.draw(gfx);
                } else if self.list_open && self.focus == Focus::Subject {
                    let d = ((wp.0 >> 16) & 0xFFFF) as u16 as i16 as i32 / 120;
                    let max = self.hits.len().saturating_sub(VIS) as i32;
                    self.top = (self.top as i32 - d).clamp(0, max) as usize;
                    self.draw(gfx);
                }
                Some(LRESULT(0))
            }
            WM_TIMER if wp.0 == T_ANIM => {
                let more = self.tick();
                // the old contents have shrunk away: the next mode takes over and grows in
                if self.switch.as_ref().is_some_and(|sw| sw.t >= 1.0) {
                    if let Some(sw) = self.switch.take() {
                        self.mode = sw.to;
                        self.sw = 0.0;
                        self.show(gfx, 1.0, true);
                    }
                }
                if !more {
                    unsafe {
                        let _ = KillTimer(Some(self.hwnd), T_ANIM);
                    }
                    self.timer_on = false;
                }
                if !self.open && self.anim <= 0.0 {
                    self.finish_hide();
                } else {
                    self.draw(gfx);
                }
                Some(LRESULT(0))
            }
            WM_TIMER if wp.0 == T_ALT => {
                // Alt let go: the window chosen with Tab opens (with no Tab, the switcher stays for the arrows)
                if !alt_down() {
                    unsafe {
                        let _ = KillTimer(Some(self.hwnd), T_ALT);
                    }
                    if self.open && self.mode == Mode::Windows && self.wn_tabs > 0 {
                        self.windows_commit();
                    }
                }
                Some(LRESULT(0))
            }
            WM_TIMER if wp.0 == T_FLASH => {
                unsafe {
                    let _ = KillTimer(Some(self.hwnd), T_FLASH);
                }
                if self.flashing {
                    self.hide();
                }
                Some(LRESULT(0))
            }
            WM_TIMER if wp.0 == T_RESCAN => {
                unsafe {
                    let _ = KillTimer(Some(self.hwnd), T_RESCAN);
                }
                self.rescan_apps();
                Some(LRESULT(0))
            }
            WM_TIMER if wp.0 == T_PICK => {
                if self.visible && self.pick.is_some() {
                    self.sample();
                    self.draw(gfx);
                }
                Some(LRESULT(0))
            }
            WM_TIMER if wp.0 == T_LIVE => {
                if self.visible && self.live {
                    let (x, y, w, h) = self.win_px;
                    if let Some(img) = capture_blur(x, y, w, h, self.cfg.blur) {
                        if let Some(old) = self.blur_img.replace(img) {
                            self.res.forget(old.id);
                        }
                        self.draw(gfx);
                    }
                }
                Some(LRESULT(0))
            }
            WM_TIMER if wp.0 == T_ICON => {
                unsafe {
                    let _ = KillTimer(Some(self.hwnd), T_ICON);
                }
                self.draw(gfx);
                Some(LRESULT(0))
            }
            WM_TIMER if wp.0 == T_TRIM => {
                // quiet for a while: hand the drawing memory back, then forget everything later
                unsafe {
                    let _ = KillTimer(Some(self.hwnd), T_TRIM);
                    SetTimer(Some(self.hwnd), T_FREE, 280_000, None);
                }
                self.icons.clear();
                self.host.reset_dc(gfx);
                self.res.trim_bitmaps();
                Some(LRESULT(0))
            }
            WM_TIMER if wp.0 == T_FREE => {
                unsafe {
                    let _ = KillTimer(Some(self.hwnd), T_FREE);
                }
                if !self.open && !self.visible {
                    self.free_me = true;
                }
                Some(LRESULT(0))
            }
            WM_PROCS => {
                if let Some(v) = self.procs_slot.lock().ok().and_then(|mut g| g.take()) {
                    if self.sampler.is_some() {
                        self.snap = Some(v);
                    }
                }
                if self.open && self.mode == Mode::Kill && self.sampler.is_some() {
                    self.refresh_procs();
                    self.draw(gfx);
                }
                Some(LRESULT(0))
            }
            WM_RATES => {
                if self.open && self.mode == Mode::Search && self.pending.is_none() {
                    let (sel, top) = (self.sel, self.top);
                    self.search();
                    self.sel = sel.min(self.hits.len().saturating_sub(1));
                    self.top = top.min(self.hits.len().saturating_sub(VIS));
                    self.draw(gfx);
                }
                Some(LRESULT(0))
            }
            WM_AI => {
                if self.ai_poll() && self.open && self.mode == Mode::Ai {
                    self.draw(gfx);
                }
                Some(LRESULT(0))
            }
            WM_PREVIEW => {
                let got: Vec<(isize, ImageData)> =
                    self.wn_slot.lock().map(|mut g| std::mem::take(&mut *g)).unwrap_or_default();
                for (h, img) in got {
                    if self.wn.iter().any(|w| w.hwnd == h) {
                        if let Some(old) = self.wn_prev.insert(h, img) {
                            self.res.forget(old.id);
                        }
                    } else {
                        self.res.forget(img.id);
                    }
                }
                if self.open && self.mode == Mode::Windows {
                    self.draw(gfx);
                }
                Some(LRESULT(0))
            }
            WM_INDEX => {
                if let Some(v) = self.apps_slot.lock().ok().and_then(|mut g| g.take()) {
                    self.apps = v;
                    self.apps_ready = true;
                }
                if let Some(v) = self.files_slot.lock().ok().and_then(|mut g| g.take()) {
                    self.files = v;
                }
                if self.open {
                    let (sel, top) = (self.sel, self.top);
                    self.search();
                    self.sel = sel.min(self.hits.len().saturating_sub(1));
                    self.top = top.min(self.hits.len().saturating_sub(VIS));
                    self.draw(gfx);
                }
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    // -- drawing -----------------------------------------------------------------------------------------

    /// Shell icons of what is on screen, a few per frame so typing never stalls. True while more are due.
    fn load_icons(&mut self) -> bool {
        // the dock's icons changed (a new picture, other pins): look at them again
        let ver = crate::dock::icon_gen();
        if ver != self.icon_gen {
            self.icon_gen = ver;
            self.icons.clear();
        }
        let mut want: Vec<(String, String)> = Vec::new();
        // the file on the left: its picture or its first lines
        let shown = self.pending.as_ref().map(|(_, h)| h).or_else(|| self.cur());
        if let Some(h) = shown.filter(|h| h.kind == Kind::File) {
            let path = h.path.clone();
            self.load_peek(&path);
        }
        if let Some((_, h)) = &self.pending {
            if h.kind.has_icon() {
                want.push((h.path.clone(), h.name.clone()));
            }
        }
        if let Some(h) = self.cur() {
            if h.kind.has_icon() {
                want.push((h.path.clone(), h.name.clone()));
            }
        }
        if self.mode == Mode::Kill && !self.query.trim().is_empty() {
            for h in self.hits.iter().skip(self.sel & !1).take(2) {
                want.push((h.path.clone(), h.name.clone()));
            }
        }
        if self.mode == Mode::Windows {
            for w in self.wn.iter().skip(self.wn_x as usize).take(SW_VIS + 2) {
                if !w.exe.is_empty() {
                    want.push((w.exe.clone(), String::new()));
                }
            }
        }
        if self.list_open && self.focus == Focus::Subject {
            for h in self.hits.iter().skip(self.top).take(VIS) {
                if h.kind.has_icon() {
                    want.push((h.path.clone(), h.name.clone()));
                }
            }
        }
        want.retain(|(k, _)| !self.icons.contains_key(k));
        if want.is_empty() {
            return false;
        }
        if self.icons.len() > 96 {
            self.icons.clear();
        }
        let mut loaded = 0;
        let mut missing = false;
        for (k, name) in want {
            if self.icons.contains_key(&k) {
                continue;
            }
            if loaded >= 3 {
                missing = true;
                break;
            }
            loaded += 1;
            let img = crate::dock::icon_for_path(&k, &name);
            self.icons.insert(k, img);
        }
        missing
    }

    fn draw(&mut self, gfx: &Gfx) {
        if !self.visible || self.win_px.2 == 0 {
            return;
        }
        if self.needs_anim() {
            self.kick();
        }
        let missing = self.load_icons();
        if missing {
            unsafe {
                SetTimer(Some(self.hwnd), T_ICON, 6, None);
            }
        }
        LIGHT.store(self.cfg.light, std::sync::atomic::Ordering::Relaxed);
        let light = self.cfg.light;
        let sc = self.scale;
        let e = logic::ease_out(self.anim);
        let open_s = self.open_scale();
        let shown = self.rows_len().min(self.vis());
        // clicks land on the glass only: the region reaches as far down as the list is (or is about to be)
        self.set_region(self.lh.max(self.list_target()), self.kw.max(self.kw_target()), self.anim < 1.0);
        let cur = self.cur().cloned();
        let vtop = self.view_top();
        let wx = self.wx.clone();
        let mut ai_max = self.ai.max_scroll;
        let cins = self.card_inset();
        let solo = self.solo_rect();
        // the system's backdrop is never used by the launcher: the blur is drawn from the screen copy
        if self.live {
            // the glass grows from 95% around its middle while it opens; the blur follows it
            let s0 = self.open_scale();
            let (ax, ay) = (W * 0.5, FRAME_H * 0.5);
            let map = |r: Rect| {
                Rect::new(
                    (E + ax + (r.x - ax) * s0) * sc,
                    (ay + (r.y - ay) * s0) * sc,
                    r.w * s0 * sc,
                    r.h * s0 * sc,
                )
            };
            let grown = E * self.kw.clamp(0.0, 1.0);
            let live_card = Rect::new(-grown + cins, 0.0, W + 2.0 * grown - 2.0 * cins, FRAME_H);
            self.host.set_backdrop_rounded(map(live_card), CARD_R * s0 * sc, e);
            let lh = self.lh.max(0.0);
            if lh > 8.0 {
                let r = LIST_R.min(lh * 0.5) * s0 * sc;
                self.host.set_backdrop2_rounded(map(Rect::new(0.0, LIST_Y, W, lh)), r, e);
            } else {
                self.host.set_backdrop2_rounded(Rect::new(0.0, 0.0, 0.0, 0.0), 0.0, 0.0);
            }
        } else {
            self.host.set_backdrop_rounded(Rect::new(0.0, 0.0, 0.0, 0.0), 0.0, 0.0);
            self.host.set_backdrop2_rounded(Rect::new(0.0, 0.0, 0.0, 0.0), 0.0, 0.0);
        }
        let mut ai_goal = None;
        let mut ai_agoal = None;
        let (dc, _) = match self.host.begin_draw() {
            Ok(v) => v,
            Err(err) => {
                crate::log!("launcher: begin_draw failed: {err}");
                return;
            }
        };
        {
            let mut p = Painter::new(dc, gfx, &mut self.res, sc, E * sc, 0.0);
            p.clear();
            p.alpha = e;
            // opening: appears where it is, fading in while growing from 95% around its middle; closing runs it backwards
            let tr = Tr { s0: open_s, top: 0.0, dx: 0.0, k: 1.0, kx: 0.0, ky: 0.0 };
            // changing the mode: the old contents swell a little, then shrink away into the middle of their
            // box; the new ones grow out of the middle (with a small overshoot)
            let (pop, fade) = match &self.switch {
                Some(sw) => {
                    let t = sw.t.clamp(0.0, 1.0);
                    let k = if t < 0.25 {
                        1.0 + 0.05 * logic::ease_out(t / 0.25)
                    } else {
                        let u = (t - 0.25) / 0.75;
                        1.05 * (1.0 - u * u * u)
                    };
                    (k, 1.0 - ((t - 0.5) / 0.5).clamp(0.0, 1.0))
                }
                None => {
                    let sw = self.sw.clamp(0.0, 1.0);
                    (0.45 + 0.55 * logic::ease_out(sw), logic::ease_out((sw * 2.0).min(1.0)))
                }
            };
            let ct = Tr { ..tr };
            card_xf(&mut p, &tr);
            // with three boxes the card is wider than the two-box card, by the same amount on each side
            let grown = E * self.kw.clamp(0.0, 1.0);
            let card = Rect::new(-grown + cins, 0.0, W + 2.0 * grown - 2.0 * cins, FRAME_H);
            // the list is a second piece of glass that floats below the boxes
            let panel = Rect::new(0.0, LIST_Y, W, self.lh.max(0.0));
            let dark = self.cfg.dark;
            let glass = self.cfg.glass;
            let gl = Glass {
                img: self.blur_img.as_ref().filter(|_| glass),
                whole: Rect::new(-E, 0.0, self.win_px.2 as f32 / sc, self.win_px.3 as f32 / sc),
                light,
                dark,
                tint: self.cfg.tint,
                glass,
                native: self.live,
                // the rim is the dock's (its thickness and brightness come from the dock settings)
                rim: (self.cfg.rim_width, if light { 0.75 } else { self.cfg.rim_bright }),
            };
            draw_glass(&mut p, &gl, card, CARD_R);
            if panel.h > 8.0 {
                draw_glass(&mut p, &gl, panel, LIST_R.min(panel.h * 0.5));
            }
            let sub = Rect::new(PAD - grown, PAD, BOX, BOX);
            let act = Rect::new(PAD + BOX + GAP - grown, PAD, BOX, BOX);
            // the Run box: the left box stretches over the whole card while the right one fades away
            let rw = self.rw.clamp(0.0, 1.0);
            let sub_box = Rect::new(sub.x + (solo.x - sub.x) * rw, sub.y, sub.w + (solo.w - sub.w) * rw, BOX);
            // the third box (Kill mode) comes in on the right as the card grows
            let third = Rect::new(PAD + 2.0 * (BOX + GAP) - grown, PAD, BOX, BOX);
            // the focus glides from one box to the other
            let ft = self.focus_t;
            let kill = self.mode == Mode::Kill;
            let found = kill && !self.query.trim().is_empty() && !self.hits.is_empty();
            let ons = if kill {
                [
                    self.arm_t,
                    if found && self.sel % 2 == 0 { 1.0 } else { 0.0 },
                    if found && self.sel % 2 == 1 { 1.0 } else { 0.0 },
                ]
            } else {
                [1.0 - ft, ft, 0.0]
            };
            let n_boxes = if self.mode == Mode::Windows {
                0
            } else if self.kw > 0.01 {
                3
            } else {
                2
            };
            let keep_a = p.alpha;
            for (i, (r, on)) in
                [(sub_box, ons[0]), (act, ons[1]), (third, ons[2])].into_iter().enumerate().take(n_boxes)
            {
                if i == 1 {
                    if rw > 0.995 {
                        continue;
                    }
                    p.alpha = keep_a * (1.0 - rw);
                }
                if i == 2 {
                    p.alpha = keep_a * self.kw.clamp(0.0, 1.0);
                    p.push_clip(card);
                }
                // the Kill box turns red when Enter was pressed once
                let (hot, hot_rim) = if kill && i == 0 {
                    (Color::rgba(0.50, 0.12, 0.13, 0.90), Color::rgba(0.95, 0.45, 0.45, 0.85))
                } else {
                    (sel_fill(light, 1.0), sel_rim(light))
                };
                let w = 1.0 + 0.6 * on;
                // the boxes are more solid than the clearer glass around them; the focus turns them blue
                let (calm, calm_rim) = if light {
                    (Color::white(0.42), Color::white(0.55))
                } else {
                    (Color::rgba(0.03, 0.04, 0.07, 0.28 + 0.30 * dark), Color::white(0.15))
                };
                p.fill_rounded(r, BOX_R, calm.lerp(hot, on));
                if !light {
                    // a faint sheen on the calm box
                    p.fill_rounded(r, BOX_R, Color::white(0.06 * (1.0 - on)));
                }
                p.stroke_rounded(r.inset(w * 0.5), BOX_R - w * 0.5, w, calm_rim.lerp(hot_rim, on));
                if i == 2 {
                    p.pop_clip();
                    p.alpha = keep_a;
                }
            }
            p.alpha = keep_a;
            // choosing a target: the left box keeps the thing the action is for
            let subj = self.pending.as_ref().map(|(_, h)| h).or(cur.as_ref());
            let icon = subj.and_then(|h| self.icons.get(&h.path)).and_then(|o| o.as_ref());
            let peek = subj.filter(|h| h.kind == Kind::File).and_then(|h| {
                match self.icons.get(&format!("prev:{}", h.path)).and_then(|o| o.as_ref()) {
                    Some(img) => Some(Peek::Img(img)),
                    None => self.texts.get(&h.path).filter(|t| !t.trim().is_empty()).map(|t| Peek::Text(t)),
                }
            });
            // the contents of both boxes pop (and fade) when the mode changes: while they do, each box's
            // contents are drawn on their own, scaled around the middle of that box
            let keep = p.alpha;
            p.alpha = keep * fade;
            let popping = (pop - 1.0).abs() > 0.001;
            let kill_ui = kill && self.pick.is_none();
            let kv = KillView {
                cpu: self.cpu_s,
                ram: self.ram_s,
                ram_used: self.snap.as_ref().map_or(0, |s| s.ram_used),
                ram_total: self.snap.as_ref().map_or(0, |s| s.ram_total),
                ready: self.snap.is_some(),
                typing: !self.query.trim().is_empty(),
                page: [self.hits.get(self.sel & !1), self.hits.get((self.sel & !1) + 1)],
                icons: &self.icons,
                armed: cur
                    .as_ref()
                    .filter(|h| self.armed.as_deref() == Some(h.path.as_str()))
                    .map(|h| h.name.as_str()),
                pop: self.pop_s,
                pop_a: self.pop_a,
            };
            let (both, both3, one) = ([Some(sub_box), Some(act)], [Some(sub_box), Some(act), Some(third)], [None]);
            let passes: &[Option<Rect>] = if !popping {
                &one
            } else if kill_ui {
                &both3
            } else {
                &both
            };
            for &only in passes {
                let ctb = match only {
                    Some(r) => Tr { k: pop, kx: r.cx(), ky: r.cy(), ..ct },
                    None => Tr { ..ct },
                };
                let subject = Look {
                    cur: subj,
                    icon,
                    wx: wx.as_deref(),
                    pop: self.pop_s,
                    ul: self.ul,
                    tr: &ctb,
                    ready: self.apps_ready,
                    clip: self.mode == Mode::Clip,
                    peek,
                };
                card_xf(&mut p, &ctb);
                if let Some(r) = only {
                    p.push_clip(r);
                }
                if let Some(pk) = self.pick.as_ref() {
                    draw_pick(&mut p, sub, act, pk);
                } else if kill_ui {
                    draw_kill(&mut p, [sub, act, third], &kv, only, &ctb);
                } else if self.mode == Mode::Windows {
                    let view = WinView {
                        wins: &self.wn,
                        prev: &self.wn_prev,
                        icons: &self.icons,
                        sel: self.wn_sel,
                        scroll: self.wn_x,
                        pop: self.pop_s,
                    };
                    draw_windows(&mut p, card, &view);
                } else if self.mode == Mode::Run {
                    // the command, centred and sized like a question to Gemini
                    let qh = text_hash(&self.query);
                    if qh != self.ai.q.fit_for || self.query.is_empty() {
                        ai_goal =
                            Some((qh, fit_text(&mut p, &self.query, solo.w - 28.0, BOX - 44.0, &Q_SIZES, 600, true)));
                    }
                    let view = RunView {
                        query: &self.query,
                        pop: self.pop_s,
                        q: (self.ai.q.sz, self.ai.q.y, self.ai.q.center),
                        err: self.run_err,
                        recent: !self.run_hist.is_empty(),
                    };
                    draw_run(&mut p, sub_box, &view);
                } else if matches!(self.mode, Mode::Ai | Mode::Notes) {
                    let notes = self.mode == Mode::Notes;
                    let qh = text_hash(&self.query);
                    if qh != self.ai.q.fit_for || self.query.is_empty() {
                        ai_goal =
                            Some((qh, fit_text(&mut p, &self.query, BOX - 28.0, BOX - 28.0, &Q_SIZES, 600, true)));
                    }
                    let ah = text_hash(&self.ai.text);
                    if !notes && (ah != self.ai.a.fit_for || self.ai.text.is_empty()) {
                        ai_agoal =
                            Some((ah, fit_text(&mut p, &self.ai.text, BOX - 36.0, BOX - 28.0, &A_SIZES, 500, false)));
                    }
                    let view = AiView {
                        query: &self.query,
                        state: self.ai.state,
                        text: if notes { &self.notes_text } else { &self.ai.text },
                        notes,
                        scroll: self.ai.scroll,
                        pop_s: self.pop_s,
                        pop_a: self.pop_a,
                        q: (self.ai.q.sz, self.ai.q.y, self.ai.q.center),
                        a: (self.ai.a.sz, self.ai.a.y, self.ai.a.center),
                    };
                    ai_max = draw_ai(&mut p, sub, act, &view);
                } else {
                    // the right box (what to do with the match) is only there when asked for: until then the
                    // left box is the whole card, with a line saying what Enter does
                    draw_subject(&mut p, sub_box, &subject);
                    if rw > 0.5 && !self.one_box && self.pending.is_none() && peek.is_none() {
                        if let Some(h) =
                            cur.as_ref().filter(|h| !matches!(h.kind, Kind::Clip | Kind::Weather | Kind::Day))
                        {
                            let acts = actions(h);
                            let (g1, g2) =
                                if sub_box.w < 250.0 { (" ", "  \u{b7}  ") } else { ("  ", "    \u{b7}    ") };
                            let line = if acts.len() > 1 {
                                format!("Enter{g1}{}{g2}Ctrl+K{g1}more", acts[0].label())
                            } else {
                                format!("Enter{g1}{}", acts[0].label())
                            };
                            let a_in = p.alpha;
                            p.alpha = a_in * ((rw - 0.5) * 2.0);
                            let hint = Rect::new(sub_box.x + 10.0, sub_box.bottom() - 26.0, sub_box.w - 20.0, 16.0);
                            p.text(&line, hint, &TextStyle::new(10.5, white(0.50)).center());
                            p.alpha = a_in;
                        }
                    }
                    if rw < 0.995 {
                        let a_in = p.alpha;
                        p.alpha = a_in * (1.0 - rw);
                        match &self.pending {
                            Some((a, _)) => {
                                let target_icon =
                                    cur.as_ref().and_then(|h| self.icons.get(&h.path)).and_then(|o| o.as_ref());
                                draw_target(
                                    &mut p,
                                    act,
                                    cur.as_ref(),
                                    target_icon,
                                    a.label().trim_end_matches('\u{2026}'),
                                    self.pop_a,
                                    &ctb,
                                );
                            }
                            None => {
                                let armed =
                                    cur.as_ref().is_some_and(|h| self.armed.as_deref() == Some(h.path.as_str()));
                                draw_action(&mut p, act, cur.as_ref(), self.act, armed, self.pop_a, &ctb);
                            }
                        }
                        p.alpha = a_in;
                    }
                }
                if only.is_some() {
                    p.pop_clip();
                }
            }
            p.alpha = keep;
            card_xf(&mut p, &tr);
            if self.lh > 1.0 {
                p.push_clip(panel);
                let view = ListView {
                    hits: &self.hits,
                    icons: &self.icons,
                    query: &self.query,
                    focus: self.focus,
                    sel: self.sel,
                    top: vtop,
                    act: self.act,
                    shown,
                    lh: self.lh,
                    sel_y: self.sel_y,
                    ul: self.ul,
                    wx: wx.as_deref(),
                    clip: self.mode == Mode::Clip,
                    kill: self.mode == Mode::Kill,
                    run: self.mode == Mode::Run,
                };
                draw_list(&mut p, &view);
                p.pop_clip();
            }
        }
        if let Err(err) = self.host.end_draw() {
            crate::log!("launcher: end_draw failed: {err}");
        }
        self.res.end_frame();
        self.ai.max_scroll = ai_max;
        if let Some((hash, fit)) = ai_goal {
            self.ai.q.set(hash, fit);
        }
        if let Some((hash, fit)) = ai_agoal {
            self.ai.a.set(hash, fit);
        }
        if self.ai.moving() {
            self.kick();
        }
        self.ai.scroll = self.ai.scroll.min(ai_max);
    }
}

impl Drop for Launcher {
    fn drop(&mut self) {
        self.ai.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        unsafe {
            for t in [T_ANIM, T_ICON, T_TRIM, T_FREE, T_LIVE, T_PICK, T_RESCAN, T_ALT] {
                let _ = KillTimer(Some(self.hwnd), t);
            }
            unwatch_clicks();
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

// -- pieces of the picture -----------------------------------------------------------------------------

/// Light glass look (dark writing) or dark glass look (white writing); set at the start of every frame.
static LIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// The colour of writing, icons and soft highlights: white on dark glass, dark ink on light glass.
fn white(a: f32) -> Color {
    if LIGHT.load(std::sync::atomic::Ordering::Relaxed) {
        Color::rgba(0.10, 0.12, 0.18, a)
    } else {
        Color::white(a)
    }
}

/// The blue of whatever is selected; it is translucent, so the glass shows through. `k` scales how strong.
fn sel_fill(light: bool, k: f32) -> Color {
    if light {
        Color::rgba(0.50, 0.74, 1.0, 0.62 * k)
    } else {
        // #15334b, nearly opaque so it shows as that colour
        Color::rgba(0.082, 0.200, 0.294, 0.88 * k)
    }
}

/// The edge of something selected.
fn sel_rim(light: bool) -> Color {
    if light {
        Color::rgba(0.70, 0.86, 1.0, 0.95)
    } else {
        Color::rgba(0.36, 0.56, 0.78, 0.80)
    }
}

/// What the glass pieces (the box card and the floating list) have in common.
struct Glass<'a> {
    /// The blurred screen copy; none = a plain solid piece.
    img: Option<&'a ImageData>,
    /// Where that copy sits (the whole window).
    whole: Rect,
    light: bool,
    dark: f32,
    /// A small dark-grey wash on the dark look (from Settings).
    tint: f32,
    glass: bool,
    /// Windows blurs what is behind the glass live (so no picture is drawn, only the tint).
    native: bool,
    /// Rim thickness and brightness.
    rim: (f32, f32),
}

/// One piece of liquid glass: the blurred screen, magnified toward the edge like thick glass, a light
/// tint (clearer than the boxes that sit on it), the rim and the light along the inside of the edge.
fn draw_glass(p: &mut Painter, g: &Glass, r: Rect, radius: f32) {
    match g.img {
        Some(img) => {
            let (cx, cy) = (r.cx(), r.cy());
            for (k, inset) in [(1.07, 0.0), (1.045, 5.0), (1.022, 9.0), (1.0, 13.0)] {
                let dest =
                    Rect::new(cx + (g.whole.x - cx) * k, cy + (g.whole.y - cy) * k, g.whole.w * k, g.whole.h * k);
                p.image_window(img, dest, r.inset(inset), (radius - inset).max(8.0), 1.0);
            }
            let tint = if g.light {
                Color::rgba(0.95, 0.97, 1.0, 0.12 + 0.40 * g.dark)
            } else {
                Color::rgba(0.03, 0.03, 0.05, 0.62 * g.dark)
            };
            p.fill_rounded(r, radius, tint);
            if !g.light && g.tint > 0.005 {
                // the small dark-grey wash
                p.fill_rounded(r, radius, Color::rgba(0.07, 0.07, 0.08, g.tint));
            }
        }
        // Windows' own blur behind: only the tint is drawn here, never less than a clear body so the glass
        // never turns see-through (even where the blur is switched off in Windows)
        None if g.native => {
            // Windows' blur is already smoky by itself, so the tints here are much lighter than over the
            // plain screen picture (stacked, they made the glass look solid)
            let tint = if g.light {
                Color::rgba(0.95, 0.97, 1.0, (0.12 + 0.40 * g.dark) * 0.5)
            } else {
                Color::rgba(0.03, 0.03, 0.05, 0.62 * g.dark * 0.4)
            };
            p.fill_rounded(r, radius, tint);
            if !g.light && g.tint > 0.005 {
                p.fill_rounded(r, radius, Color::rgba(0.07, 0.07, 0.08, g.tint * 0.4));
            }
        }
        // glass off, or no picture of the screen: a solid piece
        None if g.light => p.fill_rounded(r, radius, Color::rgba(0.93, 0.95, 0.98, 0.96)),
        None => p.fill_rounded(r, radius, Color::rgba(0.05, 0.05, 0.07, g.dark.max(0.9))),
    }
    if g.glass {
        let (rw, rb) = g.rim;
        if rw > 0.05 && rb > 0.005 {
            p.stroke_rounded(r.inset(rw * 0.5), (radius - rw * 0.5).max(0.0), rw, Color::white(rb));
        }
        // light catching the inside of the edge
        let inner = rw + 1.5;
        p.stroke_rounded(r.inset(inner), (radius - inner).max(0.0), 1.0, Color::white(0.09));
    } else {
        let rim = if g.light { Color::white(0.6) } else { Color::white(0.12) };
        p.stroke_rounded(r.inset(0.5), radius - 0.5, 1.0, rim);
    }
}

/// The icon of an empty search box: a letter and a text cursor.
fn type_glyph(p: &mut Painter, cx: f32, cy: f32, c: Color) {
    p.text("A", Rect::centered(cx - 10.0, cy, 44.0, 56.0), &TextStyle::new(46.0, c).center());
    let x = cx + 17.0;
    p.fill_rect(Rect::new(x - 0.75, cy - 18.0, 1.5, 36.0), c);
    p.fill_rect(Rect::new(x - 5.0, cy - 18.0, 10.0, 1.5), c);
    p.fill_rect(Rect::new(x - 5.0, cy + 16.5, 10.0, 1.5), c);
}

/// How the card sits at this moment (it fades and grows a little while opening and closing).
struct Tr {
    s0: f32,
    top: f32,
    /// Sideways shift.
    dx: f32,
    /// An extra scale (the pop when the mode changes) around the point (`kx`, `ky`).
    k: f32,
    kx: f32,
    ky: f32,
}

/// The point the card grows from.
const ANCHOR: (f32, f32) = (W * 0.5, FRAME_H * 0.5);

fn card_xf(p: &mut Painter, t: &Tr) {
    let dx = t.s0 * (1.0 - t.k) * t.kx + (1.0 - t.s0) * ANCHOR.0 + t.dx;
    let dy = t.s0 * (1.0 - t.k) * t.ky + (1.0 - t.s0) * ANCHOR.1 + t.top;
    p.set_local(t.s0 * t.k, 0.0, 0.0, dx, dy);
}

/// Scale one thing by `k` around (cx, cy), on top of the card's own transform.
fn pop_xf(p: &mut Painter, t: &Tr, k: f32, cx: f32, cy: f32) {
    let dx = t.s0 * t.k * (1.0 - k) * cx + t.s0 * (1.0 - t.k) * t.kx + (1.0 - t.s0) * ANCHOR.0 + t.dx;
    let dy = t.s0 * t.k * (1.0 - k) * cy + t.s0 * (1.0 - t.k) * t.ky + (1.0 - t.s0) * ANCHOR.1 + t.top;
    p.set_local(t.s0 * t.k * k, 0.0, 0.0, dx, dy);
}

/// A thin line under the letters that matched what was typed. It grows from left to right.
fn underline(p: &mut Painter, text: &str, spans: &[(usize, usize)], r: Rect, st: &TextStyle, center: bool, grow: f32) {
    if spans.is_empty() || grow <= 0.01 {
        return;
    }
    let (tw, th) = p.measure(text, st);
    if tw > r.w {
        // the end is cut off: the positions would be wrong
        return;
    }
    let x0 = if center { r.x + (r.w - tw) * 0.5 } else { r.x };
    let y = r.cy() + th * 0.38;
    let mut parts: Vec<(f32, f32)> = Vec::with_capacity(spans.len());
    for &(a, b) in spans {
        let (a, b) = logic::snap(text, a, b);
        let xa = if a == 0 { 0.0 } else { p.measure(&text[..a], st).0 };
        parts.push((xa, p.measure(&text[..b], st).0));
    }
    let total: f32 = parts.iter().map(|(a, b)| b - a).sum();
    let mut left = total * logic::ease_out(grow);
    for (xa, xb) in parts {
        let w = (xb - xa).max(1.0).min(left.max(0.0));
        left -= xb - xa;
        if w > 0.5 {
            p.fill_rect(Rect::new(x0 + xa, y, w, 1.3), white(0.95));
        }
    }
}

/// The picture of a match: its icon, or a stand-in while the icon loads.
fn draw_thumb(p: &mut Painter, r: Rect, h: &Hit, img: Option<&ImageData>, wx: Option<&Brief>) {
    match h.kind {
        Kind::Calc => {
            p.fill_rounded(r, r.w * 0.26, white(0.14));
            p.text("=", r, &TextStyle::new(r.w * 0.55, white(0.92)).semibold().center());
        }
        Kind::Url | Kind::Web => p.icon(Icon::Globe, r.inset(r.w * 0.1), white(0.9)),
        Kind::System => {
            p.fill_rounded(r, r.w * 0.26, white(0.12));
            p.icon(sys_icon(h.idx), r.inset(r.w * 0.22), white(0.9));
        }
        Kind::Emoji => p.text(&h.path, r, &TextStyle::new(r.w * 0.72, white(0.95)).center()),
        Kind::Run => {
            p.fill_rounded(r, r.w * 0.26, white(0.12));
            p.icon(Icon::Play, r.inset(r.w * 0.27), white(0.9));
        }
        Kind::Clip => match img {
            Some(i) => p.image(i, r, 5.0, 1.0),
            None => {
                p.fill_rounded(r, r.w * 0.26, white(0.12));
                let glyph = match h.idx {
                    1 => Icon::Link,
                    2 => Icon::Image,
                    3 => Icon::File,
                    _ => Icon::Clipboard,
                };
                p.icon(glyph, r.inset(r.w * 0.22), white(0.85));
            }
        },
        Kind::Weather => match wx {
            Some(b) => b.draw_icon(p, r),
            None => p.icon(Icon::Cloud, r.inset(r.w * 0.1), white(0.9)),
        },
        Kind::Day => match wx {
            Some(b) => b.day_icon(h.idx, p, r),
            None => p.icon(Icon::Cloud, r.inset(r.w * 0.1), white(0.9)),
        },
        _ => match img {
            Some(i) => p.image(i, r, 0.0, 1.0),
            None => {
                // no icon yet: a glass tile with the same glyph the rest of the app uses
                p.fill_rounded(r, r.w * 0.26, white(0.12));
                let glyph = match h.kind {
                    Kind::Folder => Icon::Folder,
                    Kind::File => Icon::File,
                    _ => Icon::Grid,
                };
                p.icon(glyph, r.inset(r.w * 0.22), white(0.85));
            }
        },
    }
}

/// The program the default web browser starts (read from the registry), so a combo can show its icon.
fn default_browser_exe() -> Option<String> {
    use windows::Win32::Foundation::WIN32_ERROR;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY, HKEY_CLASSES_ROOT, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
    let read = |root: HKEY, sub: &str, val: PCWSTR| -> Option<String> {
        let sub: Vec<u16> = sub.encode_utf16().chain(Some(0)).collect();
        let mut buf = [0u16; 520];
        let mut sz = (buf.len() * 2) as u32;
        let r = unsafe {
            RegGetValueW(
                root,
                PCWSTR(sub.as_ptr()),
                val,
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr() as *mut _),
                Some(&mut sz),
            )
        };
        (r == WIN32_ERROR(0)).then(|| crate::util::from_wide(&buf)).filter(|s| !s.is_empty())
    };
    let key = r"Software\Microsoft\Windows\Shell\Associations\UrlAssociations\https\UserChoice";
    let prog = read(HKEY_CURRENT_USER, key, w!("ProgId"))?;
    let cmd = read(HKEY_CLASSES_ROOT, &format!(r"{prog}\shell\open\command"), PCWSTR::null())?;
    let cmd = crate::dock::expand_env(cmd.trim());
    let exe = match cmd.strip_prefix('"') {
        Some(rest) => rest.split('"').next()?.to_string(),
        None => cmd[..cmd.to_ascii_lowercase().find(".exe")? + 4].to_string(),
    };
    Path::new(&exe).is_file().then_some(exe)
}

/// What the left box shows right now.
struct Look<'a> {
    cur: Option<&'a Hit>,
    icon: Option<&'a ImageData>,
    wx: Option<&'a Brief>,
    pop: f32,
    ul: f32,
    tr: &'a Tr,
    ready: bool,
    clip: bool,
    /// The picture or the first lines of the file.
    peek: Option<Peek<'a>>,
}

#[derive(Clone, Copy)]
enum Peek<'a> {
    Img(&'a ImageData),
    Text(&'a str),
}

fn draw_subject(p: &mut Painter, r: Rect, v: &Look) {
    let cx = r.cx();
    let Some(h) = v.cur else {
        if v.clip {
            p.icon(Icon::Clipboard, Rect::centered(cx, r.y + 68.0, 52.0, 52.0), white(0.55));
            p.text(
                "Nothing copied yet",
                Rect::new(r.x + 10.0, r.y + 118.0, r.w - 20.0, 20.0),
                &TextStyle::new(11.5, white(0.80)).semibold().center(),
            );
            return;
        }
        type_glyph(p, cx, r.y + 68.0, white(0.62));
        let msg = if v.ready { "Type…" } else { "Getting ready…" };
        p.text(
            msg,
            Rect::new(r.x + 10.0, r.y + 118.0, r.w - 20.0, 20.0),
            &TextStyle::new(11.5, white(0.80)).semibold().center(),
        );
        return;
    };
    if h.kind == Kind::Clip {
        draw_clip_preview(p, r, h, v);
        return;
    }
    if let (Kind::File, Some(pk)) = (h.kind, &v.peek) {
        draw_peek(p, r, h, pk, v);
        return;
    }
    let a0 = p.alpha;
    // a new match pops in: the picture swells from 80% with a little overshoot, the words fade up
    let fade = logic::ease_out((v.pop * 1.6).min(1.0));
    let k = 0.50 + 0.50 * logic::ease_out(v.pop);
    let slide = (1.0 - fade) * 5.0;
    if h.kind == Kind::Weather {
        let icon = Rect::centered(cx, r.y + 44.0, 60.0, 60.0);
        pop_xf(p, v.tr, k, cx, icon.cy());
        p.alpha = a0 * (0.3 + 0.7 * fade);
        draw_thumb(p, icon, h, None, v.wx);
        card_xf(p, v.tr);
        p.alpha = a0 * fade;
        p.text(
            &h.name,
            Rect::new(r.x + 8.0, r.y + 72.0 + slide, r.w - 16.0, 44.0),
            &TextStyle::new(36.0, white(0.96)).semibold().center(),
        );
        p.text(
            &h.detail,
            Rect::new(r.x + 10.0, r.y + 116.0 + slide, r.w - 20.0, 20.0),
            &TextStyle::new(12.0, white(0.78)).semibold().center(),
        );
        p.alpha = a0;
        return;
    }
    let shown = if h.kind == Kind::Calc { h.detail.as_str() } else { h.name.as_str() };
    if h.kind == Kind::Calc {
        p.alpha = a0 * fade;
        p.text(
            &h.name,
            Rect::new(r.x + 8.0, r.y + 28.0, r.w - 16.0, 66.0),
            &TextStyle::new(36.0, white(0.96)).semibold().center(),
        );
    } else {
        let ir = Rect::centered(cx, r.y + 68.0, 72.0, 72.0);
        pop_xf(p, v.tr, k, cx, ir.cy());
        p.alpha = a0 * (0.3 + 0.7 * fade);
        draw_thumb(p, ir, h, v.icon, v.wx);
        card_xf(p, v.tr);
    }
    p.alpha = a0 * (0.35 + 0.65 * fade);
    let name_r = Rect::new(r.x + 10.0, r.y + 116.0 + slide, r.w - 20.0, 22.0);
    let st = TextStyle::new(11.5, white(0.90)).semibold().center();
    p.text(shown, name_r, &st);
    if h.kind.has_icon() {
        underline(p, shown, &h.spans, name_r, &st, true, v.ul);
    }
    if h.kind == Kind::Day {
        p.alpha = a0 * fade;
        p.text(
            &h.detail,
            Rect::new(r.x + 10.0, r.y + 138.0, r.w - 20.0, 18.0),
            &TextStyle::new(10.5, white(0.55)).center(),
        );
    }
    p.alpha = a0;
}

/// The left box in clipboard mode: the picture, or the first lines of the words.
fn draw_clip_preview(p: &mut Painter, r: Rect, h: &Hit, v: &Look) {
    let a0 = p.alpha;
    let fade = logic::ease_out((v.pop * 1.6).min(1.0));
    p.alpha = a0 * (0.35 + 0.65 * fade);
    match (h.idx, v.icon) {
        (2, Some(img)) => {
            p.image_zoom(img, Rect::new(r.x + 14.0, r.y + 14.0, r.w - 28.0, r.h - 50.0), 10.0, 1.0, 0.0);
        }
        (2, None) => p.icon(Icon::Image, Rect::centered(r.cx(), r.y + 66.0, 52.0, 52.0), white(0.55)),
        _ => {
            // big and centred while short, smaller and left-aligned from the top as it grows
            let text = clip_id(&h.path).and_then(crate::cliphist::preview).unwrap_or_else(|| h.name.clone());
            let text: String = text.trim().chars().take(600).collect();
            let view = Rect::new(r.x + 14.0, r.y + 14.0, r.w - 28.0, r.h - 50.0);
            let (sz, y, centered) = fit_text(p, &text, view.w, view.h, &C_SIZES, 500, false);
            let mut st = TextStyle::new(sz, white(0.90)).medium().top().wrap();
            if centered {
                st = st.center();
            }
            let hh = p.measure_wrapped(&text, &st, view.w);
            p.push_clip(view);
            p.text(&text, Rect::new(view.x, view.y + y, view.w, hh), &st);
            p.pop_clip();
        }
    }
    p.alpha = a0 * fade;
    p.text(
        &h.detail,
        Rect::new(r.x + 10.0, r.bottom() - 32.0, r.w - 20.0, 18.0),
        &TextStyle::new(10.5, white(0.55)).center(),
    );
    p.alpha = a0;
}

/// What the two boxes of the Gemini mode show.
struct AiView<'a> {
    query: &'a str,
    state: AiState,
    text: &'a str,
    /// Notes mode: the answer box lists the saved notes, small.
    notes: bool,
    scroll: f32,
    pop_s: f32,
    pop_a: f32,
    /// Size, top edge and centring of the question and of the answer (they glide).
    q: (f32, f32, bool),
    a: (f32, f32, bool),
}

struct WinView<'a> {
    wins: &'a [crate::switcher::Win],
    prev: &'a HashMap<isize, ImageData>,
    icons: &'a HashMap<String, Option<ImageData>>,
    sel: usize,
    /// How far the row is scrolled, in cards.
    scroll: f32,
    pop: f32,
}

/// Is the Alt key down right now?
fn alt_down() -> bool {
    let state = unsafe { GetAsyncKeyState(VK_MENU.0 as i32) };
    state as u16 & 0x8000 != 0
}

// -- click outside --------------------------------------------------------------------------------------

/// The launcher window the click watcher reports to, and the watcher itself (0 = none). The watcher
/// exists only while the launcher is open, so a closed launcher costs nothing.
static CLICK_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
static CLICK_HOOK: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// Tell the launcher about every mouse click that lands outside it (the click itself goes on to where it was aimed).
fn watch_clicks(hwnd: HWND) {
    use std::sync::atomic::Ordering::Relaxed;
    if CLICK_HOOK.load(Relaxed) != 0 {
        return;
    }
    CLICK_HWND.store(hwnd.0 as isize, Relaxed);
    if let Ok(h) = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(click_hook), None, 0) } {
        CLICK_HOOK.store(h.0 as isize, Relaxed);
    }
}

fn unwatch_clicks() {
    use std::sync::atomic::Ordering::Relaxed;
    let h = CLICK_HOOK.swap(0, Relaxed);
    CLICK_HWND.store(0, Relaxed);
    if h != 0 {
        unsafe {
            let _ = UnhookWindowsHookEx(HHOOK(h as *mut _));
        }
    }
}

unsafe extern "system" fn click_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    use std::sync::atomic::Ordering::Relaxed;
    if code == 0 && matches!(wp.0 as u32, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN) {
        let me = CLICK_HWND.load(Relaxed);
        if me != 0 {
            let hwnd = HWND(me as *mut _);
            let info = &*(lp.0 as *const MSLLHOOKSTRUCT);
            if WindowFromPoint(info.pt) != hwnd {
                let _ = PostMessageW(Some(hwnd), WM_OUTSIDE, WPARAM(0), LPARAM(0));
            }
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

/// Is a mouse button held down right now?
fn mouse_down() -> bool {
    [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON].iter().any(|k| unsafe { GetAsyncKeyState(k.0 as i32) } as u16 & 0x8000 != 0)
}

/// Where card `i` of `n` sits (the row is `scroll` cards along; with fewer cards than fit they are centred).
fn sw_rect(i: usize, n: usize, scroll: f32) -> Rect {
    let shown = n.clamp(1, SW_VIS) as f32;
    let span = shown * SW_CW + (shown - 1.0) * SW_GAP;
    let x0 = -E + (W3 - span) * 0.5;
    Rect::new(x0 + (i as f32 - scroll) * (SW_CW + SW_GAP), PAD, SW_CW, SW_CH)
}

/// The pictures of windows are taken on a thread of their own, the ones in `order` one after the other; each
/// is handed over with `WM_PREVIEW`.
fn spawn_previews(order: Vec<(isize, bool)>, slot: PrevSlot, cancel: Arc<AtomicBool>, hwnd: SendHwnd) {
    use std::sync::atomic::Ordering::Relaxed;
    let _ = std::thread::Builder::new().name("launcher-previews".into()).spawn(move || {
        for (h, minimized) in order {
            if cancel.load(Relaxed) {
                return;
            }
            if minimized {
                continue;
            }
            if let Some(img) = crate::switcher::capture(h, 360) {
                if cancel.load(Relaxed) {
                    return;
                }
                if let Ok(mut g) = slot.lock() {
                    g.push((h, img));
                }
                unsafe {
                    let _ = PostMessageW(Some(hwnd.hwnd()), WM_PREVIEW, WPARAM(0), LPARAM(0));
                }
            }
        }
    });
}

/// The window switcher: a row of cards, each with a picture of the window, its title and its program.
fn draw_windows(p: &mut Painter, card: Rect, v: &WinView) {
    let a0 = p.alpha;
    let light = LIGHT.load(std::sync::atomic::Ordering::Relaxed);
    p.alpha = a0 * (0.35 + 0.65 * logic::ease_out((v.pop * 1.6).min(1.0)));
    let n = v.wins.len();
    if n == 0 {
        p.icon(Icon::Grid, Rect::centered(card.cx(), card.y + 74.0, 52.0, 52.0), white(0.62));
        let hint = TextStyle::new(11.5, white(0.80)).semibold().center();
        p.text("No open windows", Rect::new(card.x + 10.0, card.y + 112.0, card.w - 20.0, 20.0), &hint);
        p.alpha = a0;
        return;
    }
    let (calm, calm_rim) = if light {
        (Color::white(0.42), Color::white(0.55))
    } else {
        (Color::rgba(0.03, 0.04, 0.07, 0.50), Color::white(0.15))
    };
    p.push_clip(card);
    for (i, w) in v.wins.iter().enumerate() {
        let r = sw_rect(i, n, v.scroll);
        if r.right() < card.x || r.x > card.right() {
            continue;
        }
        let on = i == v.sel;
        let lw = if on { 1.6 } else { 1.0 };
        p.fill_rounded(r, SW_R, if on { sel_fill(light, 1.0) } else { calm });
        p.stroke_rounded(r.inset(lw * 0.5), SW_R - lw * 0.5, lw, if on { sel_rim(light) } else { calm_rim });
        // the picture of the window, whole, on a darker ground
        let view = Rect::new(r.x + 10.0, r.y + 10.0, r.w - 20.0, 88.0);
        p.fill_rounded(view, 12.0, Color::rgba(0.0, 0.0, 0.0, if light { 0.10 } else { 0.28 }));
        let icon = v.icons.get(&w.exe).and_then(|o| o.as_ref());
        match v.prev.get(&w.hwnd) {
            Some(img) => {
                let k = (view.w / img.width as f32).min(view.h / img.height as f32);
                let fit = Rect::centered(view.cx(), view.cy(), img.width as f32 * k, img.height as f32 * k);
                p.image(img, fit, 8.0, 1.0);
            }
            None => {
                if let Some(ic) = icon {
                    p.image(ic, Rect::centered(view.cx(), view.cy(), 44.0, 44.0), 0.0, 0.85);
                }
            }
        }
        if let Some(ic) = icon {
            p.image(ic, Rect::new(r.x + 12.0, r.y + 106.0, 20.0, 20.0), 0.0, 1.0);
        }
        let tx = r.x + 38.0;
        let tw = r.right() - tx - 12.0;
        let title = if w.title.is_empty() { "(no title)" } else { w.title.as_str() };
        p.text(title, Rect::new(tx, r.y + 104.0, tw, 18.0), &TextStyle::new(12.0, white(0.95)).semibold());
        let sub = if w.minimized { format!("{} \u{b7} minimised", w.app()) } else { w.app() };
        p.text(&sub, Rect::new(tx, r.y + 122.0, tw, 16.0), &TextStyle::new(10.5, white(0.55)));
    }
    p.pop_clip();
    let note = format!("{} of {}  \u{b7}  Tab or \u{2190} \u{2192} choose  \u{b7}  Enter opens", v.sel + 1, n);
    let foot = Rect::new(card.x + 10.0, card.bottom() - 24.0, card.w - 20.0, 16.0);
    p.text(&note, foot, &TextStyle::new(10.5, white(0.45)).center());
    p.alpha = a0;
}

struct RunView<'a> {
    query: &'a str,
    pop: f32,
    /// Size, top edge and centring of the command (they glide).
    q: (f32, f32, bool),
    /// The last command could not be started.
    err: bool,
    /// There are recent commands to list with the Down arrow.
    recent: bool,
}

/// The Run box: one wide box with the command in the middle, centred like a question to Gemini.
fn draw_run(p: &mut Painter, r: Rect, v: &RunView) {
    let a0 = p.alpha;
    p.alpha = a0 * (0.35 + 0.65 * logic::ease_out((v.pop * 1.6).min(1.0)));
    if v.query.is_empty() {
        p.icon(Icon::Play, Rect::centered(r.cx(), r.y + 62.0, 46.0, 46.0), white(0.62));
        let hint = TextStyle::new(11.5, white(0.80)).semibold().center();
        p.text("Type a command to run\u{2026}", Rect::new(r.x + 10.0, r.y + 106.0, r.w - 20.0, 20.0), &hint);
    } else {
        let view = Rect::new(r.x + 14.0, r.y + 14.0, r.w - 28.0, r.h - 44.0);
        let mut st = TextStyle::new(v.q.0, white(0.94)).semibold().top().wrap();
        if v.q.2 {
            st = st.center();
        }
        let h = p.measure_wrapped(v.query, &st, view.w);
        p.push_clip(view);
        p.text(v.query, Rect::new(view.x, view.y + v.q.1, view.w, h), &st);
        p.pop_clip();
    }
    let (note, k) = if v.err {
        ("Couldn\u{2019}t find that \u{b7} check the name and try again", 0.90)
    } else if v.recent {
        ("Enter runs it  \u{b7}  \u{2193} recent commands", 0.45)
    } else {
        ("Enter runs it", 0.45)
    };
    p.text(note, Rect::new(r.x + 10.0, r.bottom() - 26.0, r.w - 20.0, 16.0), &TextStyle::new(10.5, white(k)).center());
    p.alpha = a0;
}

/// Gemini mode: the question on the left, the answer on the right. Returns how far the answer can scroll.
fn draw_ai(p: &mut Painter, sub: Rect, act: Rect, v: &AiView) -> f32 {
    let a0 = p.alpha;
    let centered = TextStyle::new(11.5, white(0.80)).semibold().center();
    // left: what was typed, the end of it kept in view
    p.alpha = a0 * (0.35 + 0.65 * logic::ease_out((v.pop_s * 1.6).min(1.0)));
    if v.query.is_empty() {
        let (glyph, hint) =
            if v.notes { (Icon::Edit, "Jot a note\u{2026}") } else { (Icon::Sparkle, "Ask Gemini\u{2026}") };
        p.icon(glyph, Rect::centered(sub.cx(), sub.y + 68.0, 52.0, 52.0), white(0.62));
        p.text(hint, Rect::new(sub.x + 10.0, sub.y + 118.0, sub.w - 20.0, 20.0), &centered);
    } else {
        let view = Rect::new(sub.x + 14.0, sub.y + 14.0, sub.w - 28.0, sub.h - 28.0);
        let mut st = TextStyle::new(v.q.0, white(0.94)).semibold().top().wrap();
        if v.q.2 {
            st = st.center();
        }
        let h = p.measure_wrapped(v.query, &st, view.w);
        p.push_clip(view);
        p.text(v.query, Rect::new(view.x, view.y + v.q.1, view.w, h), &st);
        p.pop_clip();
    }
    // right: the answer
    p.alpha = a0 * (0.35 + 0.65 * logic::ease_out((v.pop_a * 1.6).min(1.0)));
    let mut max = 0.0;
    if v.text.is_empty() {
        let (icon, msg) = match v.state {
            AiState::Waiting => (Icon::Sparkle, "Thinking\u{2026}"),
            _ if v.notes => (Icon::Clipboard, "Enter saves it"),
            _ => (Icon::Chat, "Press Enter to ask"),
        };
        p.icon(icon, Rect::centered(act.cx(), act.y + 68.0, 52.0, 52.0), white(0.62));
        p.text(msg, Rect::new(act.x + 10.0, act.y + 118.0, act.w - 20.0, 20.0), &centered);
    } else {
        let view = Rect::new(act.x + 14.0, act.y + 14.0, act.w - 28.0, act.h - 28.0);
        let (asz, ay, acenter) = if v.notes { (12.5, 0.0, false) } else { v.a };
        let mut st = TextStyle::new(asz, white(0.92)).medium().top().wrap();
        if acenter {
            st = st.center();
        }
        let text_w = view.w - 8.0;
        let h = p.measure_wrapped(v.text, &st, text_w);
        max = (h - view.h).max(0.0);
        let off = v.scroll.clamp(0.0, max);
        p.push_clip(view);
        p.text(v.text, Rect::new(view.x, view.y + ay - off, text_w, h), &st);
        p.pop_clip();
        if max > 0.0 {
            // a thin bar shows there is more
            let track = view.h - 8.0;
            let bar = (track * view.h / h).max(18.0);
            let y = view.y + 4.0 + (track - bar) * off / max;
            p.fill_rounded(Rect::new(act.right() - 10.0, y, 3.0, bar), 1.5, white(0.35));
        }
    }
    p.alpha = a0;
    max
}

/// The glyph of a system command.
/// The colour picker: a magnified look at the pixels around the pointer on the left (the one in the
/// middle is the pick) with its hex code, and the colour written out three ways on the right.
fn draw_pick(p: &mut Painter, sub: Rect, act: Rect, pk: &Pick) {
    let n = PICK_N as usize;
    let cell = 10.0;
    let side = cell * n as f32;
    let grid = Rect::new(sub.cx() - side * 0.5, sub.y + 12.0, side, side);
    p.push_clip(grid);
    for (i, &v) in pk.cells.iter().enumerate() {
        let c = Color::rgba(
            ((v >> 16) & 0xFF) as f32 / 255.0,
            ((v >> 8) & 0xFF) as f32 / 255.0,
            (v & 0xFF) as f32 / 255.0,
            1.0,
        );
        p.fill_rect(Rect::new(grid.x + (i % n) as f32 * cell, grid.y + (i / n) as f32 * cell, cell, cell), c);
    }
    p.pop_clip();
    p.stroke_rounded(grid.expand(1.0), 3.0, 2.0, white(0.35));
    // the middle pixel is the pick: a frame in whichever of black and white shows on it
    let (r, g, b) = pk.rgb();
    let lum = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
    let mid = Rect::new(grid.x + (n / 2) as f32 * cell, grid.y + (n / 2) as f32 * cell, cell, cell);
    p.stroke_rounded(
        mid.expand(1.0),
        1.5,
        2.0,
        if lum > 140.0 { Color::rgba(0.0, 0.0, 0.0, 0.85) } else { Color::white(0.95) },
    );
    let texts = pk.texts();
    p.text(
        &texts[0].0,
        Rect::new(sub.x + 8.0, grid.y + side + 8.0, sub.w - 16.0, 28.0),
        &TextStyle::new(20.0, white(0.95)).semibold().center(),
    );
    // the three ways of writing it
    let light = LIGHT.load(std::sync::atomic::Ordering::Relaxed);
    for (i, (label, (shown, _))) in ["HEX", "RGB", "HSL"].iter().zip(texts.iter()).enumerate() {
        let row = Rect::new(act.x + 10.0, act.y + 12.0 + i as f32 * 46.0, act.w - 20.0, 42.0);
        if i == pk.row {
            p.fill_rounded(row, 12.0, sel_fill(light, 0.8));
            p.stroke_rounded(row.inset(0.5), 11.5, 1.0, sel_rim(light).lerp(Color::rgba(0.0, 0.0, 0.0, 0.0), 0.6));
        }
        p.text(label, Rect::new(row.x + 12.0, row.y + 5.0, row.w - 24.0, 14.0), &TextStyle::new(10.5, white(0.55)));
        p.text(
            shown,
            Rect::new(row.x + 12.0, row.y + 19.0, row.w - 24.0, 20.0),
            &TextStyle::new(14.0, white(0.95)).semibold(),
        );
    }
    p.text(
        "Enter copies \u{b7} Esc back",
        Rect::new(act.x + 10.0, act.y + 12.0 + 3.0 * 46.0, act.w - 20.0, 16.0),
        &TextStyle::new(10.0, white(0.5)).center(),
    );
}

fn sys_icon(i: usize) -> Icon {
    match i {
        0 => Icon::Lock,
        1 => Icon::Moon,
        2 => Icon::Moon,
        3 => Icon::Refresh,
        4 => Icon::Bolt,
        5 => Icon::Close,
        6 => Icon::Trash,
        8 => Icon::Image,
        _ => Icon::Brightness,
    }
}

/// The left box for a file: its picture, or its first lines in the biggest writing that fits.
fn draw_peek(p: &mut Painter, r: Rect, h: &Hit, pk: &Peek, v: &Look) {
    let a0 = p.alpha;
    let fade = logic::ease_out((v.pop * 1.6).min(1.0));
    p.alpha = a0 * (0.35 + 0.65 * fade);
    let view = Rect::new(r.x + 14.0, r.y + 14.0, r.w - 28.0, r.h - 50.0);
    match pk {
        Peek::Img(img) => p.image_zoom(img, view, 10.0, 1.0, 0.0),
        Peek::Text(t) => {
            let text: String = t.trim().chars().take(600).collect();
            let (sz, y, centered) = fit_text(p, &text, view.w, view.h, &C_SIZES, 500, false);
            let mut st = TextStyle::new(sz, white(0.90)).medium().top().wrap();
            if centered {
                st = st.center();
            }
            let hh = p.measure_wrapped(&text, &st, view.w);
            p.push_clip(view);
            p.text(&text, Rect::new(view.x, view.y + y, view.w, hh), &st);
            p.pop_clip();
        }
    }
    p.alpha = a0 * fade;
    p.text(
        &logic::mid(&h.name, 24),
        Rect::new(r.x + 10.0, r.bottom() - 32.0, r.w - 20.0, 18.0),
        &TextStyle::new(10.5, white(0.7)).semibold().center(),
    );
    p.alpha = a0;
}

/// The right box while choosing where an action goes: the app or folder that is picked.
fn draw_target(p: &mut Painter, r: Rect, cur: Option<&Hit>, icon: Option<&ImageData>, verb: &str, pop: f32, tr: &Tr) {
    let a0 = p.alpha;
    let Some(h) = cur else {
        p.text(
            "Type a name\u{2026}",
            Rect::new(r.x + 10.0, r.y + 118.0, r.w - 20.0, 20.0),
            &TextStyle::new(11.5, white(0.80)).semibold().center(),
        );
        return;
    };
    let fade = logic::ease_out((pop * 1.6).min(1.0));
    let k = 0.50 + 0.50 * logic::ease_out(pop);
    let ir = Rect::centered(r.cx(), r.y + 62.0, 56.0, 56.0);
    pop_xf(p, tr, k, r.cx(), ir.cy());
    p.alpha = a0 * (0.3 + 0.7 * fade);
    draw_thumb(p, ir, h, icon, None);
    card_xf(p, tr);
    p.alpha = a0 * (0.35 + 0.65 * fade);
    p.text(verb, Rect::new(r.x + 10.0, r.y + 106.0, r.w - 20.0, 16.0), &TextStyle::new(10.5, white(0.60)).center());
    p.text(
        &logic::mid(&h.name, 22),
        Rect::new(r.x + 10.0, r.y + 124.0, r.w - 20.0, 22.0),
        &TextStyle::new(11.5, white(0.90)).semibold().center(),
    );
    p.alpha = a0;
}

fn draw_action(p: &mut Painter, r: Rect, cur: Option<&Hit>, act: usize, armed: bool, pop: f32, tr: &Tr) {
    let cx = r.cx();
    let Some(h) = cur else { return };
    let acts = actions(h);
    let a = acts[act.min(acts.len() - 1)];
    let a0 = p.alpha;
    let fade = logic::ease_out((pop * 1.6).min(1.0));
    let k = 0.50 + 0.50 * logic::ease_out(pop);
    let ir = Rect::centered(cx, r.y + 68.0, 56.0, 56.0);
    pop_xf(p, tr, k, cx, ir.cy());
    p.alpha = a0 * (0.3 + 0.7 * fade);
    p.icon(if h.kind == Kind::System { sys_icon(h.idx) } else { a.icon() }, ir, white(0.92));
    card_xf(p, tr);
    p.alpha = a0 * (0.35 + 0.65 * fade);
    let slide = (1.0 - fade) * 5.0;
    // a command shows its own name, and asks again before it switches the computer off
    let label = match (h.kind, armed) {
        (_, true) if a == Act::Uninstall => "Press Enter to confirm",
        (Kind::System, true) => "Press Enter to confirm",
        (Kind::System, false) => h.name.as_str(),
        _ => a.label(),
    };
    p.text(
        label,
        Rect::new(r.x + 10.0, r.y + 116.0 + slide, r.w - 20.0, 22.0),
        &TextStyle::new(11.5, white(0.90)).semibold().center(),
    );
    p.alpha = a0;
}

/// What the three boxes of the Kill mode show.
struct KillView<'a> {
    /// Processor and memory use of the computer, as the gauges show them (0 to 1).
    cpu: f32,
    ram: f32,
    ram_used: u64,
    ram_total: u64,
    /// The programs have been looked at.
    ready: bool,
    /// Something is typed: the middle and right boxes show the programs that fit it.
    typing: bool,
    /// The two programs the middle and right box show.
    page: [Option<&'a Hit>; 2],
    icons: &'a HashMap<String, Option<ImageData>>,
    /// The selected program, when Enter was pressed once for it (its name).
    armed: Option<&'a str>,
    pop: f32,
    pop_a: f32,
}

/// The accent of the gauge bars.
fn gauge_color(frac: f32) -> Color {
    let light = LIGHT.load(std::sync::atomic::Ordering::Relaxed);
    let calm = if light { Color::rgba(0.20, 0.45, 0.85, 0.95) } else { Color::rgba(0.45, 0.70, 1.0, 0.95) };
    // nearly full: the bar turns red
    calm.lerp(Color::rgba(0.95, 0.35, 0.32, 0.95), ((frac - 0.75) / 0.2).clamp(0.0, 1.0))
}

/// A gauge box: what it measures, the big number, a line of small print and a bar.
fn draw_gauge(p: &mut Painter, r: Rect, name: &str, frac: f32, line: &str) {
    p.text(name, Rect::new(r.x + 8.0, r.y + 16.0, r.w - 16.0, 20.0), &TextStyle::new(12.0, white(0.62)).semibold().center());
    let pct = format!("{:.0}%", frac.clamp(0.0, 1.0) * 100.0);
    p.text(&pct, Rect::new(r.x + 8.0, r.y + 46.0, r.w - 16.0, 60.0), &TextStyle::new(44.0, white(0.96)).semibold().center());
    p.text(line, Rect::new(r.x + 8.0, r.y + 108.0, r.w - 16.0, 18.0), &TextStyle::new(11.5, white(0.62)).center());
    let track = Rect::new(r.x + 24.0, r.y + r.h - 34.0, r.w - 48.0, 8.0);
    p.fill_rounded(track, 4.0, white(0.16));
    let w = (track.w * frac.clamp(0.0, 1.0)).max(if frac > 0.004 { 8.0 } else { 0.0 });
    if w > 0.0 {
        p.fill_rounded(Rect::new(track.x, track.y, w, track.h), 4.0, gauge_color(frac));
    }
}

/// A program in one of the boxes: its icon, its name, and what it uses.
fn draw_proc(p: &mut Painter, r: Rect, h: &Hit, icons: &HashMap<String, Option<ImageData>>, pop: f32, tr: &Tr) {
    let cx = r.cx();
    let ir = Rect::centered(cx, r.y + 52.0, 56.0, 56.0);
    let a0 = p.alpha;
    let fade = logic::ease_out((pop * 1.6).min(1.0));
    let k = 0.50 + 0.50 * logic::ease_out(pop);
    pop_xf(p, tr, k, cx, ir.cy());
    p.alpha = a0 * (0.3 + 0.7 * fade);
    draw_thumb(p, ir, h, icons.get(&h.path).and_then(|o| o.as_ref()), None);
    card_xf(p, tr);
    p.alpha = a0 * (0.35 + 0.65 * fade);
    p.text(
        &logic::mid(&h.name, 16),
        Rect::new(r.x + 8.0, r.y + 90.0, r.w - 16.0, 24.0),
        &TextStyle::new(16.0, white(0.96)).semibold().center(),
    );
    p.text(&h.detail, Rect::new(r.x + 8.0, r.y + 116.0, r.w - 16.0, 18.0), &TextStyle::new(12.0, white(0.78)).center());
    if h.idx > 1 {
        p.text(
            &format!("{} processes", h.idx),
            Rect::new(r.x + 8.0, r.y + 136.0, r.w - 16.0, 16.0),
            &TextStyle::new(10.5, white(0.50)).center(),
        );
    }
    p.alpha = a0;
}

/// The three boxes of the Kill mode: Kill, then memory and processor use; with something typed, the
/// two programs that fit it. `only` draws just the box with that rectangle (while the contents pop).
fn draw_kill(p: &mut Painter, b: [Rect; 3], v: &KillView, only: Option<Rect>, tr: &Tr) {
    let wanted = |i: usize| only.map_or(true, |o| (o.x - b[i].x).abs() < 0.01);
    let a0 = p.alpha;
    if wanted(0) {
        let r = b[0];
        let cx = r.cx();
        let ir = Rect::centered(cx, r.y + 68.0, 56.0, 56.0);
        let k = 0.50 + 0.50 * logic::ease_out(v.pop_a);
        pop_xf(p, tr, k, cx, ir.cy());
        p.icon(Icon::Stop, ir, white(0.92));
        card_xf(p, tr);
        let (head, small) = match v.armed {
            Some(n) => ("Press Enter again".to_string(), format!("to end {}", logic::mid(n, 18))),
            None if v.typing => ("Kill".to_string(), "Enter twice to end it".to_string()),
            None => ("Kill".to_string(), "Type a program\u{2019}s name".to_string()),
        };
        let size = if v.armed.is_some() { 14.5 } else { 18.0 };
        p.text(&head, Rect::new(r.x + 8.0, r.y + 112.0, r.w - 16.0, 24.0), &TextStyle::new(size, white(0.95)).semibold().center());
        p.text(&small, Rect::new(r.x + 8.0, r.y + 138.0, r.w - 16.0, 18.0), &TextStyle::new(11.0, white(0.62)).center());
    }
    for i in 1..3 {
        if !wanted(i) {
            continue;
        }
        let r = b[i];
        if v.typing {
            match v.page[i - 1] {
                Some(h) => draw_proc(p, r, h, v.icons, v.pop, tr),
                None if i == 1 => p.text(
                    "No program found",
                    Rect::new(r.x + 10.0, r.y + 60.0, r.w - 20.0, 24.0),
                    &TextStyle::new(12.5, white(0.70)).semibold().center(),
                ),
                None => {}
            }
        } else if i == 1 {
            let line = if v.ready && v.ram_total > 0 {
                format!("{} of {}", procs::mem_text(v.ram_used), procs::mem_text(v.ram_total))
            } else {
                "\u{2026}".to_string()
            };
            draw_gauge(p, r, "Memory", v.ram, &line);
        } else {
            let line = if v.ready { "All cores together" } else { "\u{2026}" };
            draw_gauge(p, r, "Processor", v.cpu, line);
        }
    }
    p.alpha = a0;
}

/// Everything the dropped-down list needs.
struct ListView<'a> {
    hits: &'a [Hit],
    icons: &'a HashMap<String, Option<ImageData>>,
    query: &'a str,
    focus: Focus,
    sel: usize,
    top: usize,
    act: usize,
    shown: usize,
    /// How far the list has dropped (dips).
    lh: f32,
    /// The highlighted row (moves between rows).
    sel_y: f32,
    ul: f32,
    wx: Option<&'a Brief>,
    clip: bool,
    kill: bool,
    run: bool,
}

/// The list drops down: the card grows, each row fades and slides in as the edge passes it, and the
/// highlight glides from row to row.
fn draw_list(p: &mut Painter, v: &ListView) {
    let base = p.alpha;
    let dim = TextStyle::new(11.5, white(0.55));
    let head = Rect::new(PAD + 8.0, LIST_Y, W - 2.0 * (PAD + 8.0), HEAD);
    let y0 = LIST_Y + HEAD;
    let name_st = TextStyle::new(13.5, white(0.95)).semibold();
    let sub_st = TextStyle::new(10.5, white(0.5));
    let title = match v.focus {
        Focus::Action => match v.hits.get(v.sel) {
            Some(h) => format!("Actions for “{}”", logic::mid(&h.name, 30)),
            None => return,
        },
        Focus::Subject if v.clip => {
            let n = v.hits.len();
            let count = if n == 1 { "1 item".to_string() } else { format!("{n} items") };
            if v.query.trim().is_empty() {
                format!("Clipboard  ·  {count}")
            } else {
                format!("{count}  ·  “{}”", logic::mid(v.query.trim(), 24))
            }
        }
        Focus::Subject if v.run => {
            if v.query.trim().is_empty() {
                "Recent commands".to_string()
            } else {
                format!("Recent commands  \u{b7}  \u{201c}{}\u{201d}", logic::mid(v.query.trim(), 20))
            }
        }
        Focus::Subject if v.kill => {
            if v.query.trim().is_empty() {
                "Using the most memory".to_string()
            } else {
                let n = v.hits.len();
                let count = if n == 1 { "1 program".to_string() } else { format!("{n} programs") };
                format!("{count}  \u{b7}  \u{201c}{}\u{201d}", logic::mid(v.query.trim(), 24))
            }
        }
        Focus::Subject if v.query.trim().is_empty() => "Used most".to_string(),
        Focus::Subject => {
            let icons = v.hits.iter().filter(|h| h.kind.has_icon()).count();
            let n = if icons > 0 { icons } else { v.hits.len() };
            let count = if n >= KEEP {
                format!("{KEEP}+ results")
            } else if n == 1 {
                "1 result".to_string()
            } else {
                format!("{n} results")
            };
            format!("{count}  ·  “{}”", logic::mid(v.query.trim(), 24))
        }
    };
    p.alpha = base * (v.lh / HEAD).clamp(0.0, 1.0);
    p.text(&title, head, &dim);
    // the highlight
    let ha = ((v.lh - HEAD) / ROW).clamp(0.0, 1.0);
    if ha > 0.0 {
        p.alpha = base * ha;
        let hl = Rect::new(10.0, y0 + v.sel_y * ROW, W - 20.0, ROW - 2.0);
        let light = LIGHT.load(std::sync::atomic::Ordering::Relaxed);
        p.fill_rounded(hl, 14.0, sel_fill(light, 0.8));
        p.stroke_rounded(hl.inset(0.5), 13.5, 1.0, sel_rim(light).lerp(Color::rgba(0.0, 0.0, 0.0, 0.0), 0.6));
    }
    let reveal = |i: usize| ((v.lh - HEAD - i as f32 * ROW) / ROW).clamp(0.0, 1.0);
    match v.focus {
        Focus::Action => {
            let Some(h) = v.hits.get(v.sel) else { return };
            for (i, a) in actions(h).iter().enumerate().take(v.shown) {
                let ra = reveal(i);
                if ra <= 0.01 {
                    continue;
                }
                p.alpha = base * ra;
                let row = Rect::new(10.0, y0 + i as f32 * ROW + (1.0 - ra) * 8.0, W - 20.0, ROW - 2.0);
                p.icon(a.icon(), Rect::centered(row.x + 26.0, row.cy(), 22.0, 22.0), white(0.92));
                p.text(a.label(), Rect::new(row.x + 52.0, row.y, row.w - 62.0, row.h), &name_st);
            }
        }
        Focus::Subject => {
            for (i, h) in v.hits.iter().enumerate().skip(v.top).take(v.shown) {
                let ra = reveal(i - v.top);
                if ra <= 0.01 {
                    continue;
                }
                p.alpha = base * ra;
                let row = Rect::new(10.0, y0 + (i - v.top) as f32 * ROW + (1.0 - ra) * 8.0, W - 20.0, ROW - 2.0);
                let ir = Rect::new(row.x + 10.0, row.y + (row.h - 28.0) * 0.5, 28.0, 28.0);
                draw_thumb(p, ir, h, v.icons.get(&h.path).and_then(|o| o.as_ref()), v.wx);
                let name_r = Rect::new(row.x + 50.0, row.y + 4.0, row.w - 60.0, 20.0);
                p.text(&h.name, name_r, &name_st);
                if h.kind.has_icon() && i == v.sel {
                    underline(p, &h.name, &h.spans, name_r, &name_st, false, v.ul);
                } else if h.kind.has_icon() {
                    underline(p, &h.name, &h.spans, name_r, &name_st, false, 1.0);
                }
                let detail = logic::mid(&h.detail, 54);
                p.text(&detail, Rect::new(row.x + 50.0, row.y + 24.0, row.w - 60.0, 16.0), &sub_st);
            }
        }
    }
    p.alpha = base;
}

// -- window and background work --------------------------------------------------------------------------

fn create_window() -> windows::core::Result<HWND> {
    unsafe {
        let hinst = GetModuleHandleW(None)?;
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(launcher_proc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: CLASS,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        // like the dock, but it may take the keyboard focus (no WS_EX_NOACTIVATE)
        let hwnd = CreateWindowExW(
            WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            CLASS,
            w!("Dynamic Notch Launcher"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(hinst.into()),
            None,
        )?;
        let no_round = DWMWCP_DONOTROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &no_round as *const _ as _,
            std::mem::size_of_val(&no_round) as u32,
        );
        let on = BOOL(1);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_HOSTBACKDROPBRUSH, &on as *const _ as _, 4);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_EXCLUDED_FROM_PEEK, &on as *const _ as _, 4);
        let none: u32 = 0xFFFF_FFFE; // DWMWA_COLOR_NONE
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, &none as *const _ as _, 4);
        Ok(hwnd)
    }
}

extern "system" fn launcher_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let r = std::panic::catch_unwind(|| crate::window::with_app(|a| a.launcher_message(msg, wp, lp)).flatten());
    match r {
        Ok(Some(v)) => v,
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
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

/// Start something with administrator rights. Windows asks for confirmation in its own window; this
/// goes through a helper process so the notch is not held up while that question is open.
/// The screen behind the window (`w` x `h` pixels at `x`, `y`), blurred as much as `blur` (0 - 1) says.
pub fn capture_blur(x: i32, y: i32, w: i32, h: i32, blur: f32) -> Option<ImageData> {
    let shot = crate::refract::grab(x, y, w, h)?;
    // shrunk 4 times first: the blur works on a sixteenth of the pixels and still looks the same
    let (k, r) = if blur < 0.02 { (1, 0) } else { (4, (1.0 + blur * 7.0).round() as usize) };
    let (px, w2, h2) = logic::shrink_blur(&shot.px, shot.w, shot.h, k, r);
    Some(ImageData::from_pbgra(w2 as u32, h2 as u32, px))
}

/// The program behind an app match (its shortcut's target), when it is safe to stop it.
fn exe_name(path: &str) -> Option<String> {
    let target =
        if path.to_ascii_lowercase().ends_with(".exe") { path.to_string() } else { crate::dock::lnk_target(path)? };
    let name = target.rsplit(['\\', '/']).next()?.to_string();
    let l = name.to_ascii_lowercase();
    const KEEP_ALIVE: [&str; 9] = [
        "explorer.exe",
        "svchost.exe",
        "winlogon.exe",
        "csrss.exe",
        "wininit.exe",
        "services.exe",
        "lsass.exe",
        "dwm.exe",
        "taskkill.exe",
    ];
    let me = std::env::current_exe().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()));
    if !l.ends_with(".exe")
        || name.len() > 100
        || name.starts_with('-')
        || KEEP_ALIVE.contains(&l.as_str())
        || me.as_deref() == Some(l.as_str())
    {
        return None;
    }
    Some(name)
}

/// Ask Windows to close every window of the program (or end it at once when `force`).
fn taskkill(exe: &str, force: bool) -> bool {
    let mut c = std::process::Command::new("taskkill");
    if force {
        c.args(["/F", "/T"]);
    }
    c.args(["/IM", exe]).creation_flags(0x0800_0000);
    c.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    c.status().is_ok_and(|s| s.success())
}

fn is_running(exe: &str) -> bool {
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("IMAGENAME eq {exe}"), "/NH", "/FO", "CSV"])
        .creation_flags(0x0800_0000)
        .output();
    out.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).to_ascii_lowercase().contains(&exe.to_ascii_lowercase()))
}

/// Close the app politely, end it if it does not leave, then start it again. Runs in the background.
fn restart_app(exe: String, path: String) {
    std::thread::spawn(move || {
        low_priority();
        taskkill(&exe, false);
        let mut forced = false;
        for i in 0..32 {
            std::thread::sleep(std::time::Duration::from_millis(250));
            if !is_running(&exe) {
                std::thread::sleep(std::time::Duration::from_millis(300));
                let _ = std::process::Command::new("explorer.exe").arg(&path).spawn();
                return;
            }
            if i == 12 && !forced {
                forced = true;
                taskkill(&exe, true);
            }
        }
    });
}

/// The id of a clipboard history item (`clip:<id>`).
fn clip_id(path: &str) -> Option<u64> {
    path.strip_prefix("clip:")?.parse().ok()
}

// -- for the Alcove launcher (src/modules/alcove_launcher.rs): it lives inside the notch and uses the helpers of
// -- this file instead of copying them --------------------------------------------------------------------

/// The answer to a sum, a conversion or a time question: (answer, what it was asked).
pub(crate) fn bridge_answer(q: &str) -> Option<(String, String)> {
    match conv::convert(q) {
        Some(conv::Conv::Done { result, detail }) => return Some((result, detail)),
        Some(conv::Conv::Money { amount, from, to }) => {
            // only when today's rates are known already (the Tuna launcher fetches them)
            if let Some(r) = rate(&from, &to) {
                if let conv::Conv::Done { result, detail } = conv::money(amount, &from, &to, r) {
                    return Some((result, detail));
                }
            }
            return None;
        }
        None => {}
    }
    if let Some(t) = conv::time(q, unix_secs() as i64) {
        return Some(t);
    }
    logic::calc(q).map(|r| (r, q.to_string()))
}

pub(crate) fn bridge_url(q: &str) -> Option<String> {
    logic::as_url(q)
}

pub(crate) fn bridge_enc(q: &str) -> String {
    logic::enc(q)
}

pub(crate) fn bridge_mid(q: &str, max: usize) -> String {
    logic::mid(q, max)
}

/// The computer commands the typed text asks for: (place in the table, name, asks twice).
pub(crate) fn bridge_system(q: &str) -> Vec<(usize, String, bool)> {
    conv::system(q)
        .into_iter()
        .filter(|&i| i != conv::PICK_COLOR)
        .map(|i| (i, conv::SYSTEM[i].0.to_string(), conv::SYSTEM[i].2))
        .collect()
}

pub(crate) fn bridge_match(name: &str, q: &[u8], loose: bool, pos: &mut Vec<usize>) -> Option<i32> {
    logic::matched(name.as_bytes(), q, loose, pos)
}

pub(crate) fn bridge_run_system(i: usize) {
    run_system(i);
}

pub(crate) fn bridge_paste() {
    paste_soon();
}

/// Ask the program at `path` to quit (or end it at once when `force`), in the background.
pub(crate) fn bridge_quit(path: &str, force: bool) {
    if let Some(exe) = exe_name(path) {
        let _ = std::thread::Builder::new().name("launcher-quit".into()).spawn(move || {
            taskkill(&exe, force);
        });
    }
}

pub(crate) fn bridge_run_history() -> Vec<String> {
    load_run_history()
}

pub(crate) fn bridge_save_run_history(list: &[String]) {
    save_run_history(list);
}

/// The installed apps (call from a background thread).
pub(crate) fn bridge_apps() -> Vec<(String, String)> {
    low_priority();
    crate::dock::installed_apps()
}

/// The files and folders in the usual places and in `extra`, as (path, is a folder). Call from a background thread.
pub(crate) fn bridge_files(extra: &[String]) -> Vec<(String, bool)> {
    low_priority();
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("USERPROFILE") {
        for d in ["Desktop", "Documents", "Downloads", "Pictures", "Videos", "Music"] {
            roots.push(Path::new(&home).join(d));
        }
    }
    if let Some(od) = std::env::var_os("OneDrive") {
        for d in ["Desktop", "Documents", "Pictures"] {
            roots.push(Path::new(&od).join(d));
        }
    }
    roots.extend(extra.iter().map(PathBuf::from));
    let mut roots: Vec<(PathBuf, PathBuf)> =
        roots.into_iter().filter_map(|r| std::fs::canonicalize(&r).ok().map(|c| (c, r))).collect();
    roots.sort_by(|a, b| a.0.cmp(&b.0));
    roots.dedup_by(|a, b| a.0 == b.0);
    let mut out: Vec<Entry> = Vec::new();
    for (_, r) in &roots {
        walk(r, &mut out);
    }
    out.into_iter().map(|e| (e.path.to_string(), e.kind == Kind::Folder)).collect()
}

/// Press Ctrl+V a moment from now, when the window that had the focus before has it back.
fn paste_soon() {
    let _ = std::thread::Builder::new().name("launcher-paste".into()).spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(260));
        let v = VIRTUAL_KEY(0x56);
        let keys = [key_input(VK_CONTROL, false), key_input(v, false), key_input(v, true), key_input(VK_CONTROL, true)];
        unsafe {
            SendInput(&keys, size_of::<INPUT>() as i32);
        }
    });
}

fn run_as_admin(path: &str) {
    let _ = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            "Start-Process -FilePath $env:NOTCH_RUN -Verb RunAs",
        ])
        .env("NOTCH_RUN", path)
        .creation_flags(0x0800_0000)
        .spawn();
}

fn low_priority() {
    use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_LOWEST};
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_LOWEST);
    }
}

fn finished(slot: &Slot, list: Vec<Entry>, hwnd: SendHwnd) {
    if let Ok(mut g) = slot.lock() {
        *g = Some(list);
    }
    unsafe {
        let _ = PostMessageW(Some(hwnd.hwnd()), WM_INDEX, WPARAM(0), LPARAM(0));
    }
}

fn unix_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn notes_path() -> PathBuf {
    app_dir().join("notes.txt")
}

/// How many commands the Run box remembers.
const RUN_KEEP: usize = 20;

fn run_history_path() -> PathBuf {
    app_dir().join("run_history.txt")
}

/// The commands run from the Run box, newest first.
fn load_run_history() -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(run_history_path()) else { return Vec::new() };
    text.lines().map(str::trim).filter(|l| !l.is_empty()).take(RUN_KEEP).map(String::from).collect()
}

fn save_run_history(list: &[String]) {
    let _ = std::fs::write(run_history_path(), list.join("\n"));
}

/// The last twelve notes, newest first.
fn load_notes() -> Vec<(u64, String)> {
    let Ok(text) = std::fs::read_to_string(notes_path()) else { return Vec::new() };
    text.lines()
        .rev()
        .filter_map(|l| {
            let (at, note) = l.split_once('\t')?;
            Some((at.parse().ok()?, note.to_string()))
        })
        .take(12)
        .collect()
}

/// The notes as the right box shows them: each with how long ago it was written.
fn notes_text(notes: &[(u64, String)]) -> String {
    let now = unix_secs();
    notes.iter().map(|(at, t)| format!("{t}\n{}", logic::ago(now.saturating_sub(*at)))).collect::<Vec<_>>().join("\n\n")
}

fn copy_all(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for e in std::fs::read_dir(from)? {
            let e = e?;
            copy_all(&e.path(), &to.join(e.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to).map(|_| ())
    }
}

/// Take an app off the computer. A Store app is removed with PowerShell; for any other app the uninstaller
/// it registered is found by its name and started (it asks its own questions); when there is none, the
/// installed apps page of Settings opens.
fn uninstall_app(path: &str, name: &str) {
    const NO_WINDOW: u32 = 0x0800_0000;
    let store = path.strip_prefix("shell:AppsFolder\\").or_else(|| path.strip_prefix("shell:appsfolder\\"));
    let script = match store {
        Some(id) => {
            let family = id.split('!').next().unwrap_or("");
            if !family.is_empty() && family.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
                format!("Get-AppxPackage | Where-Object {{ $_.PackageFamilyName -eq '{family}' }} | Remove-AppxPackage")
            } else {
                "Start-Process 'ms-settings:appsfeatures'".to_string()
            }
        }
        None => format!(
            "$n='{}'; $k=Get-ItemProperty 'HKLM:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\*','HKLM:\\Software\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\*','HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\*' -ErrorAction SilentlyContinue | Where-Object {{ $_.DisplayName -and $_.UninstallString -and $_.DisplayName.StartsWith($n,[StringComparison]::OrdinalIgnoreCase) }} | Select-Object -First 1; if ($k) {{ Start-Process cmd.exe -WindowStyle Hidden -ArgumentList '/c',([string][char]34+$k.UninstallString+[string][char]34) }} else {{ Start-Process 'ms-settings:appsfeatures' }}",
            name.replace('\'', "''")
        ),
    };
    let _ = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(NO_WINDOW)
        .spawn();
}

/// Do a system command (its place in `conv::SYSTEM`).
fn run_system(i: usize) {
    const NO_WINDOW: u32 = 0x0800_0000;
    let (prog, args): (&str, &[&str]) = match i {
        0 => ("rundll32.exe", &["user32.dll,LockWorkStation"]),
        1 => ("rundll32.exe", &["powrprof.dll,SetSuspendState", "0,1,0"]),
        2 => ("shutdown.exe", &["/h"]),
        3 => ("shutdown.exe", &["/r", "/t", "0"]),
        4 => ("shutdown.exe", &["/s", "/t", "0"]),
        5 => ("shutdown.exe", &["/l"]),
        6 => ("powershell.exe", &["-NoProfile", "-Command", "Clear-RecycleBin -Force -ErrorAction SilentlyContinue"]),
        _ => (
            "powershell.exe",
            &[
                "-NoProfile",
                "-Command",
                "$p='HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize'; $n=1-(Get-ItemProperty $p).AppsUseLightTheme; Set-ItemProperty $p AppsUseLightTheme $n; Set-ItemProperty $p SystemUsesLightTheme $n",
            ],
        ),
    };
    let _ = std::process::Command::new(prog).args(args).creation_flags(NO_WINDOW).spawn();
}

/// Exchange rates against the US dollar, and when they were fetched.
static RATES: Mutex<Option<(Instant, HashMap<String, f64>)>> = Mutex::new(None);
static RATES_BUSY: AtomicBool = AtomicBool::new(false);

/// How many `to` one `from` is worth, from today's rates.
fn rate(from: &str, to: &str) -> Option<f64> {
    let g = RATES.lock().ok()?;
    let (at, m) = g.as_ref()?;
    if at.elapsed().as_secs() > 6 * 3600 {
        return None;
    }
    Some(m.get(to)? / m.get(from)?)
}

/// Fetch the rates in the background (once at a time); the launcher is told when they are here.
fn start_rates(hwnd: SendHwnd) {
    if RATES_BUSY.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let _ = std::thread::Builder::new().name("launcher-rates".into()).spawn(move || {
        low_priority();
        let cancel = AtomicBool::new(false);
        let got = crate::sys::http::fetch(
            "GET",
            "https://open.er-api.com/v6/latest/USD",
            &[],
            &[],
            crate::sys::http::Timeouts::DEFAULT,
            1 << 20,
            &cancel,
        );
        if let Ok((200, body)) = got {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) {
                if let Some(obj) = v.get("rates").and_then(|r| r.as_object()) {
                    let map: HashMap<String, f64> =
                        obj.iter().filter_map(|(k, v)| Some((k.clone(), v.as_f64()?))).collect();
                    if let Ok(mut g) = RATES.lock() {
                        *g = Some((Instant::now(), map));
                    }
                    unsafe {
                        let _ = PostMessageW(Some(hwnd.hwnd()), WM_RATES, WPARAM(0), LPARAM(0));
                    }
                }
            }
        }
        RATES_BUSY.store(false, std::sync::atomic::Ordering::SeqCst);
    });
}

/// Ask Gemini in the background. The answer is written into `shared` as it streams in, and the
/// launcher window is told each time.
fn spawn_ai(
    shared: Arc<Mutex<AiShared>>,
    hwnd: SendHwnd,
    seq: u64,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    cfg: GeminiCfg,
    key: String,
    question: String,
) {
    use std::sync::atomic::Ordering::Relaxed;
    let _ = std::thread::Builder::new().name("launcher-ai".into()).spawn(move || {
        low_priority();
        let body = serde_json::json!({
            "systemInstruction": { "parts": [{ "text": cfg.system_prompt }] },
            "contents": [{ "role": "user", "parts": [{ "text": question }] }],
            "generationConfig": { "maxOutputTokens": cfg.max_tokens },
        });
        let url =
            format!("{}/models/{}:streamGenerateContent?alt=sse", cfg.endpoint.trim_end_matches('/'), cfg.model.trim());
        let headers = [
            ("x-goog-api-key", key),
            ("Content-Type", "application/json".to_string()),
            ("Accept", "text/event-stream".to_string()),
        ];
        let payload = body.to_string().into_bytes();
        let write = |f: &dyn Fn(&mut AiShared)| {
            if let Ok(mut sh) = shared.lock() {
                if sh.seq == seq {
                    f(&mut sh);
                }
            }
            unsafe {
                let _ = PostMessageW(Some(hwnd.hwnd()), WM_AI, WPARAM(0), LPARAM(0));
            }
        };
        let mut parser = crate::sys::http::SseParser::default();
        let mut raw = Vec::new();
        let mut stream_error: Option<String> = None;
        let res = crate::sys::http::post_stream(&url, &headers, &payload, &cancel, |chunk| {
            if raw.len() < 16 * 1024 {
                raw.extend_from_slice(chunk);
            }
            parser.push(chunk, |data| {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else { return };
                if let Some(e) = v.get("error") {
                    stream_error =
                        Some(e.get("message").and_then(|m| m.as_str()).unwrap_or("stream error").to_string());
                    return;
                }
                if let Some(parts) = v.pointer("/candidates/0/content/parts").and_then(|p| p.as_array()) {
                    let text: String = parts.iter().filter_map(|p| p.get("text").and_then(|t| t.as_str())).collect();
                    if !text.is_empty() {
                        write(&|sh: &mut AiShared| sh.text.push_str(&text));
                    }
                }
                if let Some(reason) = v.pointer("/promptFeedback/blockReason").and_then(|r| r.as_str()) {
                    stream_error = Some(format!("Gemini would not answer this ({reason})."));
                }
            });
            !cancel.load(Relaxed)
        });
        if cancel.load(Relaxed) {
            return;
        }
        match res {
            Ok(200) if stream_error.is_none() => write(&|sh: &mut AiShared| sh.done = true),
            Ok(status) => {
                let msg = stream_error.unwrap_or_else(|| {
                    serde_json::from_slice::<serde_json::Value>(&raw)
                        .ok()
                        .and_then(|v| v.pointer("/error/message").and_then(|m| m.as_str()).map(str::to_string))
                        .unwrap_or_else(|| format!("HTTP {status}"))
                });
                let hint = match status {
                    400 | 401 | 403 => " (check your key in Settings \u{2192} Gemini)",
                    404 => " (check the model name in Settings \u{2192} Gemini)",
                    429 => " (too many questions right now, try again shortly)",
                    _ => "",
                };
                let msg = format!("{msg}{hint}");
                write(&|sh: &mut AiShared| sh.err = Some(msg.clone()));
            }
            Err(e) => write(&|sh: &mut AiShared| sh.err = Some(e.clone())),
        }
    });
}

/// The installed apps, found in the background.
fn spawn_apps(slot: Slot, hwnd: SendHwnd) {
    let _ = std::thread::Builder::new().name("launcher-apps".into()).spawn(move || {
        low_priority();
        let list: Vec<Entry> = crate::dock::installed_apps()
            .into_iter()
            .map(|(name, path)| Entry {
                path: path.into_boxed_str(),
                label: Some(name.into_boxed_str()),
                at: 0,
                kind: Kind::App,
            })
            .collect();
        crate::log!("launcher: {} apps", list.len());
        finished(&slot, list, hwnd);
    });
}

/// The files and folders in the usual places, found in the background.
fn spawn_files(slot: Slot, hwnd: SendHwnd, extra: Vec<String>) {
    let _ = std::thread::Builder::new().name("launcher-files".into()).spawn(move || {
        low_priority();
        let mut roots: Vec<PathBuf> = Vec::new();
        if let Some(home) = std::env::var_os("USERPROFILE") {
            for d in ["Desktop", "Documents", "Downloads", "Pictures", "Videos", "Music"] {
                roots.push(Path::new(&home).join(d));
            }
        }
        if let Some(od) = std::env::var_os("OneDrive") {
            for d in ["Desktop", "Documents", "Pictures"] {
                roots.push(Path::new(&od).join(d));
            }
        }
        roots.extend(extra.iter().map(PathBuf::from));
        // the same folder reached by two routes is searched once
        let mut roots: Vec<(PathBuf, PathBuf)> =
            roots.into_iter().filter_map(|r| std::fs::canonicalize(&r).ok().map(|c| (c, r))).collect();
        roots.sort_by(|a, b| a.0.cmp(&b.0));
        roots.dedup_by(|a, b| a.0 == b.0);
        let mut out: Vec<Entry> = Vec::new();
        for (_, r) in &roots {
            walk(r, &mut out);
        }
        out.shrink_to_fit();
        crate::log!("launcher: {} files and folders", out.len());
        finished(&slot, out, hwnd);
    });
}

fn walk(root: &Path, out: &mut Vec<Entry>) {
    let mut stack: Vec<(PathBuf, u8)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            if out.len() >= MAX_FILES {
                return;
            }
            let fname = e.file_name();
            let Some(name) = fname.to_str() else { continue };
            if name.starts_with('.') || name.starts_with('$') || name.starts_with("~$") {
                continue;
            }
            let Ok(md) = e.metadata() else { continue };
            let attrs = md.file_attributes();
            // hidden and system files stay out; so do junctions and links (they can loop)
            if attrs & 0x6 != 0 {
                continue;
            }
            let is_dir = md.is_dir();
            if is_dir && attrs & 0x400 != 0 {
                continue;
            }
            let path = e.path();
            let Some(ps) = path.to_str() else { continue };
            if !ps.ends_with(name) {
                continue;
            }
            out.push(Entry {
                path: ps.into(),
                label: None,
                at: (ps.len() - name.len()) as u32,
                kind: if is_dir { Kind::Folder } else { Kind::File },
            });
            if is_dir && depth < MAX_DEPTH && !SKIP_DIRS.contains(&name.to_ascii_lowercase().as_str()) {
                stack.push((path, depth + 1));
            }
        }
    }
}

// -- what was opened before ----------------------------------------------------------------------------------

fn usage_path() -> PathBuf {
    app_dir().join("launcher-usage.txt")
}

fn load_usage() -> HashMap<String, u32> {
    let mut m = HashMap::new();
    if let Ok(s) = std::fs::read_to_string(usage_path()) {
        for line in s.lines() {
            if let Some((n, path)) = line.split_once('\t') {
                if let Ok(n) = n.trim().parse::<u32>() {
                    m.insert(path.to_string(), n);
                }
            }
        }
    }
    m
}

fn learn_path() -> PathBuf {
    app_dir().join("launcher-learn.txt")
}

fn load_learn() -> HashMap<(String, String), u32> {
    let mut m = HashMap::new();
    if let Ok(s) = std::fs::read_to_string(learn_path()) {
        for line in s.lines() {
            let mut it = line.splitn(3, '\t');
            if let (Some(n), Some(typed), Some(path)) = (it.next(), it.next(), it.next()) {
                if let Ok(n) = n.trim().parse::<u32>() {
                    m.insert((typed.to_string(), path.to_string()), n);
                }
            }
        }
    }
    m
}

fn save_learn(m: &HashMap<(String, String), u32>) {
    let mut v: Vec<(&(String, String), &u32)> = m.iter().collect();
    v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    let text: String = v.into_iter().take(400).map(|((t, p), n)| format!("{n}\t{t}\t{p}\n")).collect();
    let _ = std::fs::write(learn_path(), text);
}

fn save_usage(m: &HashMap<String, u32>) {
    let mut v: Vec<(&String, &u32)> = m.iter().collect();
    v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    let text: String = v.into_iter().take(300).map(|(p, n)| format!("{n}\t{p}\n")).collect();
    let _ = std::fs::write(usage_path(), text);
}

// -- pure logic: matching, calculator, links (no Windows calls, unit-tested) ---------------------------------

// Conversions typed into the search box: units (`5 km in miles`), money (`20 usd in eur`), the time
// somewhere (`time in tokyo`, `3pm london in la`) and the system commands (`restart`, `shut down`).
// Pure text in, text out: nothing here touches the system.
mod conv {
    /// What a conversion came to.
    #[derive(Debug, PartialEq)]
    pub enum Conv {
        Done {
            result: String,
            detail: String,
        },
        /// Needs today's exchange rates: amount, from, to (upper-case codes).
        Money {
            amount: f64,
            from: String,
            to: String,
        },
    }

    /// 1 to 6 significant figures, no trailing zeros, no exponent for ordinary sizes.
    pub fn fmt(v: f64) -> String {
        if !v.is_finite() {
            return "?".into();
        }
        if v == 0.0 {
            return "0".into();
        }
        let digits = 6 - 1 - (v.abs().log10().floor() as i32).clamp(-5, 5);
        let s = format!("{:.*}", digits.clamp(0, 10) as usize, v);
        let s = if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s };
        // thousands separators in the whole part
        let (sign, rest) = s.strip_prefix('-').map_or(("", s.as_str()), |r| ("-", r));
        let (whole, frac) = rest.split_once('.').map_or((rest, ""), |(a, b)| (a, b));
        let mut out = String::new();
        for (i, c) in whole.chars().enumerate() {
            if i > 0 && (whole.len() - i) % 3 == 0 {
                out.push(',');
            }
            out.push(c);
        }
        if !frac.is_empty() {
            out.push('.');
            out.push_str(frac);
        }
        format!("{sign}{out}")
    }

    // dimension, factor to the base unit, symbol, names
    const UNITS: &[(u8, f64, &str, &str)] = &[
        (0, 0.001, "mm", "mm|millimeter|millimeters|millimetre|millimetres"),
        (0, 0.01, "cm", "cm|centimeter|centimeters|centimetre|centimetres"),
        (0, 1.0, "m", "m|meter|meters|metre|metres"),
        (0, 1000.0, "km", "km|kilometer|kilometers|kilometre|kilometres"),
        (0, 0.0254, "in", "in|inch|inches"),
        (0, 0.3048, "ft", "ft|foot|feet"),
        (0, 0.9144, "yd", "yd|yard|yards"),
        (0, 1609.344, "mi", "mi|mile|miles"),
        (0, 1852.0, "nmi", "nmi"),
        (1, 1e-6, "mg", "mg|milligram|milligrams"),
        (1, 1e-3, "g", "g|gram|grams"),
        (1, 1.0, "kg", "kg|kilo|kilos|kilogram|kilograms"),
        (1, 0.028349523125, "oz", "oz|ounce|ounces"),
        (1, 0.45359237, "lb", "lb|lbs|pound|pounds"),
        (1, 6.35029318, "st", "st|stone"),
        (1, 1000.0, "t", "t|tonne|tonnes"),
        (2, 0.001, "ml", "ml|milliliter|milliliters|millilitre|millilitres"),
        (2, 1.0, "l", "l|liter|liters|litre|litres"),
        (2, 0.00492892, "tsp", "tsp|teaspoon|teaspoons"),
        (2, 0.0147868, "tbsp", "tbsp|tablespoon|tablespoons"),
        (2, 0.0295735, "fl oz", "floz|fl oz|fluid ounce|fluid ounces"),
        (2, 0.236588, "cup", "cup|cups"),
        (2, 0.473176, "pt", "pt|pint|pints"),
        (2, 0.946353, "qt", "qt|quart|quarts"),
        (2, 3.78541, "gal", "gal|gallon|gallons"),
        (3, 1.0, "m/s", "m/s|mps"),
        (3, 0.277778, "km/h", "km/h|kmh|kph|kmph"),
        (3, 0.44704, "mph", "mph"),
        (3, 0.514444, "kn", "kn|knot|knots"),
        (4, 1.0, "B", "b|byte|bytes"),
        (4, 1e3, "KB", "kb|kilobyte|kilobytes"),
        (4, 1e6, "MB", "mb|megabyte|megabytes"),
        (4, 1e9, "GB", "gb|gigabyte|gigabytes"),
        (4, 1e12, "TB", "tb|terabyte|terabytes"),
        (4, 1024.0, "KiB", "kib"),
        (4, 1_048_576.0, "MiB", "mib"),
        (4, 1_073_741_824.0, "GiB", "gib"),
        (5, 0.001, "ms", "ms|millisecond|milliseconds"),
        (5, 1.0, "s", "s|sec|secs|second|seconds"),
        (5, 60.0, "min", "min|mins|minute|minutes"),
        (5, 3600.0, "h", "h|hr|hrs|hour|hours"),
        (5, 86400.0, "d", "d|day|days"),
        (5, 604_800.0, "wk", "wk|week|weeks"),
        (6, 1.0, "m²", "sqm|m2|m²"),
        (6, 1e6, "km²", "sqkm|km2|km²"),
        (6, 0.09290304, "ft²", "sqft|ft2|ft²"),
        (6, 4046.8564, "ac", "ac|acre|acres"),
        (6, 10000.0, "ha", "ha|hectare|hectares"),
        // temperatures are handled apart (dimension 7)
        (7, 0.0, "°C", "c|celsius|centigrade"),
        (7, 1.0, "°F", "f|fahrenheit"),
        (7, 2.0, "K", "k|kelvin"),
    ];

    fn unit(name: &str) -> Option<&'static (u8, f64, &'static str, &'static str)> {
        let n = name.trim();
        UNITS.iter().find(|u| u.3.split('|').any(|a| a == n))
    }

    const CURRENCIES: &[&str] = &[
        "USD", "EUR", "GBP", "JPY", "CAD", "AUD", "CHF", "CNY", "INR", "MXN", "BRL", "KRW", "SEK", "NOK", "DKK", "NZD",
        "SGD", "HKD", "ZAR", "TRY", "PLN", "CZK", "HUF", "ILS", "AED", "SAR", "THB", "IDR", "PHP", "MYR", "TWD", "RUB",
        "UAH", "EGP", "NGN", "VND", "ARS", "CLP", "COP", "PEN", "RON", "BGN",
    ];

    fn currency(name: &str) -> Option<String> {
        let n = name.trim();
        let code = match n {
            "dollar" | "dollars" | "bucks" | "$" => "USD",
            "euro" | "euros" | "€" => "EUR",
            "yen" | "¥" => "JPY",
            "£" | "sterling" => "GBP",
            _ => &n.to_ascii_uppercase(),
        };
        CURRENCIES.contains(&code).then(|| code.to_string())
    }

    /// "20 usd in eur" -> (20, "usd", "eur").
    fn parse(q: &str) -> Option<(f64, String, String)> {
        let mut q = q.trim().to_lowercase().replace('°', "").replace(',', "");
        // a money sign in front: "$20 in eur" -> "20 $ in eur"
        if let Some(c) = q.chars().next().filter(|c| matches!(c, '$' | '€' | '£' | '¥')) {
            let rest = q[c.len_utf8()..].trim_start();
            let end = rest.find(|ch: char| !(ch.is_ascii_digit() || ch == '.')).unwrap_or(rest.len());
            q = format!("{} {c}{}", &rest[..end], &rest[end..]);
        }
        let (lhs, rhs) = [" into ", " in ", " to ", " as ", "->", "="].iter().find_map(|s| q.split_once(s))?;
        let lhs = lhs.trim();
        let end = lhs.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+'))?;
        if end == 0 {
            return None;
        }
        let v: f64 = lhs[..end].parse().ok()?;
        let from = lhs[end..].trim();
        let to = rhs.trim();
        (!from.is_empty() && !to.is_empty()).then(|| (v, from.to_string(), to.to_string()))
    }

    pub fn convert(q: &str) -> Option<Conv> {
        let (v, from, to) = parse(q)?;
        if let (Some(a), Some(b)) = (currency(&from), currency(&to)) {
            return Some(Conv::Money { amount: v, from: a, to: b });
        }
        let (f, t) = (unit(&from)?, unit(&to)?);
        if f.0 != t.0 {
            return None;
        }
        let out = if f.0 == 7 {
            let k = match f.1 as u8 {
                0 => v + 273.15,
                1 => (v - 32.0) * 5.0 / 9.0 + 273.15,
                _ => v,
            };
            match t.1 as u8 {
                0 => k - 273.15,
                1 => (k - 273.15) * 9.0 / 5.0 + 32.0,
                _ => k,
            }
        } else {
            v * f.1 / t.1
        };
        Some(Conv::Done { result: format!("{} {}", fmt(out), t.2), detail: format!("{} {} → {}", fmt(v), f.2, t.2) })
    }

    /// The money answer once the rate is known (`rate` = how many `to` one `from` is worth).
    pub fn money(amount: f64, from: &str, to: &str, rate: f64) -> Conv {
        Conv::Done {
            result: format!("{} {to}", fmt(amount * rate)), detail: format!("{} {from} → {to}", fmt(amount))
        }
    }

    // -- time zones ---------------------------------------------------------------------------------------

    #[derive(Clone, Copy, PartialEq)]
    enum Dst {
        No,
        Us,
        Eu,
        Au,
    }

    // standard offset in minutes, daylight rule, display name, names
    const ZONES: &[(i32, Dst, &str, &str)] = &[
        (-480, Dst::Us, "Los Angeles", "la|los angeles|san francisco|sf|seattle|portland|san diego|las vegas|pst|pt|pacific|california"),
        (-420, Dst::Us, "Denver", "denver|mst|mt|mountain|salt lake city"),
        (-420, Dst::No, "Phoenix", "phoenix|arizona"),
        (-360, Dst::Us, "Chicago", "chicago|cst|ct|central|dallas|houston|austin|texas|mexico city"),
        (-300, Dst::Us, "New York", "new york|ny|nyc|est|et|eastern|boston|miami|atlanta|washington|dc|toronto|philadelphia"),
        (-540, Dst::Us, "Anchorage", "anchorage|alaska"),
        (-600, Dst::No, "Honolulu", "honolulu|hawaii"),
        (-180, Dst::No, "São Paulo", "sao paulo|são paulo|brasilia|rio|buenos aires"),
        (0, Dst::Eu, "London", "london|uk|britain|dublin|lisbon|bst|gmt"),
        (0, Dst::No, "UTC", "utc|reykjavik|iceland"),
        (60, Dst::Eu, "Paris", "paris|berlin|madrid|rome|amsterdam|brussels|vienna|zurich|stockholm|oslo|copenhagen|prague|warsaw|budapest|cet|france|germany|spain|italy"),
        (120, Dst::Eu, "Athens", "athens|helsinki|kyiv|kiev|bucharest|eet"),
        (120, Dst::No, "Cairo", "cairo|johannesburg|cape town|south africa|jerusalem|tel aviv"),
        (180, Dst::No, "Istanbul", "istanbul|moscow|riyadh|nairobi|baghdad|turkey"),
        (240, Dst::No, "Dubai", "dubai|abu dhabi|uae|baku"),
        (330, Dst::No, "Mumbai", "mumbai|delhi|india|kolkata|bangalore|ist"),
        (420, Dst::No, "Bangkok", "bangkok|jakarta|hanoi|ho chi minh|vietnam|thailand"),
        (480, Dst::No, "Singapore", "singapore|hong kong|beijing|shanghai|china|manila|taipei|kuala lumpur|perth"),
        (540, Dst::No, "Tokyo", "tokyo|japan|seoul|korea|osaka"),
        (600, Dst::Au, "Sydney", "sydney|melbourne|canberra|australia"),
        (600, Dst::No, "Brisbane", "brisbane"),
        (720, Dst::Au, "Auckland", "auckland|new zealand|wellington"),
    ];

    fn zone(name: &str) -> Option<&'static (i32, Dst, &'static str, &'static str)> {
        let n = name.trim();
        ZONES.iter().find(|z| z.3.split('|').any(|a| a == n))
    }

    fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    fn civil_from_days(z: i64) -> (i64, i64, i64) {
        let z = z + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        (if m <= 2 { y + 1 } else { y }, m, d)
    }

    /// 0 = Sunday.
    fn weekday(days: i64) -> i64 {
        (days + 4).rem_euclid(7)
    }

    fn nth_sunday(y: i64, m: i64, n: i64) -> i64 {
        let first = days_from_civil(y, m, 1);
        first + (7 - weekday(first)) % 7 + (n - 1) * 7
    }

    fn last_sunday(y: i64, m: i64) -> i64 {
        let next = if m == 12 { days_from_civil(y + 1, 1, 1) } else { days_from_civil(y, m + 1, 1) };
        let last = next - 1;
        last - weekday(last)
    }

    /// The offset from UTC in minutes at this moment (`utc` in seconds).
    fn offset(z: &(i32, Dst, &str, &str), utc: i64) -> i32 {
        let std = z.0 as i64;
        let (y, _, _) = civil_from_days(utc.div_euclid(86_400));
        let dst = match z.1 {
            Dst::No => false,
            Dst::Us => {
                let on = nth_sunday(y, 3, 2) * 86_400 + 2 * 3600 - std * 60;
                let off = nth_sunday(y, 11, 1) * 86_400 + 2 * 3600 - (std + 60) * 60;
                utc >= on && utc < off
            }
            Dst::Eu => {
                let on = last_sunday(y, 3) * 86_400 + 3600;
                let off = last_sunday(y, 10) * 86_400 + 3600;
                utc >= on && utc < off
            }
            Dst::Au => {
                let on = nth_sunday(y, 10, 1) * 86_400 + 2 * 3600 - std * 60;
                let off = nth_sunday(y, 4, 1) * 86_400 + 3 * 3600 - (std + 60) * 60;
                utc >= on || utc < off
            }
        };
        z.0 + if dst { 60 } else { 0 }
    }

    fn clock(min_of_day: i64) -> String {
        let m = min_of_day.rem_euclid(1440);
        let (h, mi) = (m / 60, m % 60);
        format!("{}:{:02} {}", if h % 12 == 0 { 12 } else { h % 12 }, mi, if h < 12 { "AM" } else { "PM" })
    }

    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

    /// "9am", "3:30 pm", "15:45" -> minutes after midnight.
    fn parse_clock(s: &str) -> Option<i64> {
        let s = s.trim();
        let (body, pm) = if let Some(b) = s.strip_suffix("pm") {
            (b.trim(), Some(true))
        } else if let Some(b) = s.strip_suffix("am") {
            (b.trim(), Some(false))
        } else {
            (s, None)
        };
        let (h, m) = body.split_once(':').map_or((body, "0"), |(a, b)| (a, b));
        let (h, m): (i64, i64) = (h.parse().ok()?, m.parse().ok()?);
        if m > 59 {
            return None;
        }
        let h = match pm {
            Some(true) if (1..=12).contains(&h) => h % 12 + 12,
            Some(false) if (1..=12).contains(&h) => h % 12,
            None if h <= 23 => h,
            _ => return None,
        };
        Some(h * 60 + m)
    }

    /// `time in tokyo`, `now in paris`, `3pm tokyo in la`. `now` is the UTC time in seconds.
    pub fn time(q: &str, now: i64) -> Option<(String, String)> {
        let q = q.trim().to_lowercase();
        for head in ["time in ", "time at ", "time ", "now in ", "what time is it in "] {
            if let Some(city) = q.strip_prefix(head) {
                let z = zone(city.trim_end_matches('?'))?;
                let local = now + offset(z, now) as i64 * 60;
                let day = DAYS[weekday(local.div_euclid(86_400)) as usize];
                return Some((clock(local.div_euclid(60)), format!("{day} in {}", z.2)));
            }
        }
        let (lhs, rhs) = [" in ", " to "].iter().find_map(|s| q.split_once(s))?;
        let (clk, from) = lhs.trim().split_once(' ')?;
        let clk = parse_clock(clk).or_else(|| {
            // "3 pm tokyo": the am/pm came as its own word
            let (n, rest) = lhs.trim().split_once(' ')?;
            let (ap, _) = rest.split_once(' ')?;
            parse_clock(&format!("{n}{ap}"))
        })?;
        // the zone is whatever follows the time (and a separate "pm"/"am" word)
        let from = from.trim().trim_start_matches("pm ").trim_start_matches("am ").trim();
        let (zf, zt) = (zone(from)?, zone(rhs.trim())?);
        // which day: today in the source zone
        let today = (now + offset(zf, now) as i64 * 60).div_euclid(86_400);
        let utc = today * 86_400 + clk * 60 - offset(zf, now) as i64 * 60;
        let local = utc + offset(zt, utc) as i64 * 60;
        let shift = local.div_euclid(86_400) - today;
        let note = match shift {
            -1 => " (yesterday)",
            1 => " (tomorrow)",
            _ => "",
        };
        Some((format!("{}{note}", clock(local.div_euclid(60))), format!("{} {} → {}", clock(clk), zf.2, zt.2)))
    }

    // -- system commands --------------------------------------------------------------------------------

    /// name, typed names, needs a second Enter to confirm
    pub const SYSTEM: &[(&str, &str, bool)] = &[
        ("Lock", "lock|lock screen|lock pc", false),
        ("Sleep", "sleep|suspend|standby", false),
        ("Hibernate", "hibernate", true),
        ("Restart", "restart|reboot|restart pc|restart computer", true),
        ("Shut down", "shut down|shutdown|power off|turn off|shut down pc|turn off pc", true),
        ("Sign out", "sign out|signout|log out|logout|log off|logoff", true),
        ("Empty Recycle Bin", "empty recycle bin|empty trash|recycle bin|recycle|trash|bin|empty bin", true),
        ("Toggle dark mode", "dark mode|light mode|toggle dark mode|toggle theme|theme", false),
        (
            "Pick a color",
            "color pick|color picker|colour pick|colour picker|pick color|pick colour|eyedropper|eye dropper",
            false,
        ),
    ];

    /// The place of the colour picker in `SYSTEM`: it opens inside the launcher instead of running.
    pub const PICK_COLOR: usize = 8;

    /// `#RRGGBB`.
    pub fn hex(c: (u8, u8, u8)) -> String {
        format!("#{:02X}{:02X}{:02X}", c.0, c.1, c.2)
    }

    /// Hue (degrees), saturation and lightness (percent), rounded.
    pub fn hsl(c: (u8, u8, u8)) -> (u32, u32, u32) {
        let (r, g, b) = (c.0 as f32 / 255.0, c.1 as f32 / 255.0, c.2 as f32 / 255.0);
        let (mx, mn) = (r.max(g).max(b), r.min(g).min(b));
        let l = (mx + mn) * 0.5;
        let d = mx - mn;
        if d < 1e-6 {
            return (0, 0, (l * 100.0).round() as u32);
        }
        let s = d / (1.0 - (2.0 * l - 1.0).abs());
        let h = if mx == r {
            ((g - b) / d).rem_euclid(6.0)
        } else if mx == g {
            (b - r) / d + 2.0
        } else {
            (r - g) / d + 4.0
        };
        (((h * 60.0).round() as u32) % 360, (s * 100.0).round() as u32, (l * 100.0).round() as u32)
    }

    #[cfg(test)]
    mod colour_tests {
        use super::*;

        #[test]
        fn colours_are_written_out() {
            assert_eq!(hex((255, 136, 0)), "#FF8800");
            assert_eq!(hex((0, 0, 0)), "#000000");
            assert_eq!(hsl((255, 136, 0)), (32, 100, 50));
            assert_eq!(hsl((0, 0, 255)), (240, 100, 50));
            assert_eq!(hsl((255, 0, 0)), (0, 100, 50));
            assert_eq!(hsl((128, 128, 128)), (0, 0, 50));
            assert_eq!(hsl((255, 255, 255)), (0, 0, 100));
            assert_eq!(hsl((0, 0, 0)), (0, 0, 0));
            // a red with a touch of blue sits just below the top of the wheel; a hair more rounds to the top
            assert_eq!(hsl((255, 0, 3)).0, 359);
            assert_eq!(hsl((255, 0, 1)).0, 0);
            assert_eq!(system("color pick"), vec![PICK_COLOR]);
            assert_eq!(system("colo"), vec![PICK_COLOR]);
            assert_eq!(system("eyedr"), vec![PICK_COLOR]);
            assert!(system("co").is_empty());
        }
    }

    /// The commands the typed text asks for (at least three letters, the start of what they are called).
    pub fn system(q: &str) -> Vec<usize> {
        let q = q.trim().to_lowercase();
        if q.chars().count() < 3 {
            return Vec::new();
        }
        SYSTEM
            .iter()
            .enumerate()
            .filter(|(_, s)| s.1.split('|').any(|a| a.starts_with(&q) || a == q))
            .map(|(i, _)| i)
            .collect()
    }
}

// The emoji picker's list: the picture, then the words it can be found by (the first is its name).
mod emoji {
    pub const LIST: &[(&str, &str)] = &[
        ("😀", "grinning happy smile face"),
        ("😂", "joy laughing tears funny lol"),
        ("🤣", "rofl rolling floor laughing"),
        ("😊", "blush smiling happy"),
        ("😍", "heart eyes love"),
        ("🥰", "smiling hearts love adore"),
        ("😘", "kiss blowing heart"),
        ("😎", "cool sunglasses"),
        ("🤩", "star struck wow excited"),
        ("🥳", "party celebrate birthday"),
        ("😉", "wink"),
        ("😁", "beaming grin teeth"),
        ("😅", "sweat smile nervous relief"),
        ("😆", "squinting laugh"),
        ("🙂", "slight smile"),
        ("🙃", "upside down silly"),
        ("😇", "angel innocent halo"),
        ("🤔", "thinking hmm"),
        ("🤨", "raised eyebrow skeptical"),
        ("😐", "neutral meh"),
        ("😑", "expressionless blank"),
        ("😏", "smirk"),
        ("😒", "unamused annoyed"),
        ("🙄", "eye roll"),
        ("😬", "grimace awkward"),
        ("😴", "sleeping zzz tired"),
        ("😪", "sleepy"),
        ("🤤", "drooling yum"),
        ("😋", "yum delicious tasty"),
        ("😛", "tongue out playful"),
        ("😜", "wink tongue crazy"),
        ("🤪", "zany wild crazy"),
        ("😢", "cry sad tear"),
        ("😭", "sob crying loud"),
        ("😤", "triumph steam huff"),
        ("😠", "angry mad"),
        ("😡", "rage furious red"),
        ("🤬", "cursing swearing"),
        ("😱", "scream scared shock"),
        ("😨", "fearful afraid"),
        ("😰", "anxious sweat worry"),
        ("😳", "flushed embarrassed"),
        ("🥺", "pleading puppy eyes"),
        ("😮", "open mouth surprised"),
        ("😲", "astonished shocked"),
        ("🤯", "mind blown exploding head"),
        ("🥵", "hot sweating"),
        ("🥶", "cold freezing"),
        ("🤢", "nauseated sick"),
        ("🤮", "vomit sick"),
        ("🤧", "sneeze sick"),
        ("😷", "mask sick medical"),
        ("🤒", "thermometer sick fever"),
        ("🤕", "bandage hurt injured"),
        ("🤗", "hug hugging"),
        ("🤫", "shush quiet secret"),
        ("🤭", "giggle hand over mouth"),
        ("🫡", "salute"),
        ("🫠", "melting"),
        ("😈", "devil smiling imp"),
        ("👿", "angry devil"),
        ("💀", "skull dead"),
        ("☠️", "skull crossbones danger"),
        ("👻", "ghost halloween"),
        ("👽", "alien"),
        ("🤖", "robot"),
        ("💩", "poop"),
        ("🙈", "see no evil monkey"),
        ("🙉", "hear no evil monkey"),
        ("🙊", "speak no evil monkey"),
        ("👍", "thumbs up yes good like ok"),
        ("👎", "thumbs down no bad dislike"),
        ("👌", "ok hand perfect"),
        ("✌️", "victory peace"),
        ("🤞", "crossed fingers luck"),
        ("🤟", "love you hand"),
        ("🤘", "rock on horns"),
        ("👋", "wave hello hi bye"),
        ("🤚", "raised back hand"),
        ("✋", "raised hand stop high five"),
        ("🖐️", "hand fingers splayed"),
        ("👏", "clap applause bravo"),
        ("🙌", "raised hands praise celebrate"),
        ("🙏", "pray thanks please folded hands"),
        ("🤝", "handshake deal agreement"),
        ("💪", "muscle strong flex bicep"),
        ("👈", "point left"),
        ("👉", "point right"),
        ("👆", "point up"),
        ("👇", "point down"),
        ("☝️", "index up one"),
        ("🖕", "middle finger"),
        ("✍️", "writing hand"),
        ("🤳", "selfie"),
        ("👀", "eyes looking"),
        ("👁️", "eye"),
        ("👂", "ear listen"),
        ("👃", "nose smell"),
        ("👄", "mouth lips"),
        ("🧠", "brain smart"),
        ("🫀", "heart organ anatomical"),
        ("❤️", "red heart love"),
        ("🧡", "orange heart"),
        ("💛", "yellow heart"),
        ("💚", "green heart"),
        ("💙", "blue heart"),
        ("💜", "purple heart"),
        ("🖤", "black heart"),
        ("🤍", "white heart"),
        ("🤎", "brown heart"),
        ("💔", "broken heart sad"),
        ("❣️", "heart exclamation"),
        ("💕", "two hearts love"),
        ("💞", "revolving hearts"),
        ("💖", "sparkling heart"),
        ("💗", "growing heart"),
        ("💘", "heart arrow cupid"),
        ("💝", "heart ribbon gift"),
        ("💯", "hundred 100 perfect score"),
        ("🔥", "fire hot lit flame"),
        ("✨", "sparkles shiny new magic"),
        ("⭐", "star"),
        ("🌟", "glowing star"),
        ("💫", "dizzy star"),
        ("⚡", "lightning bolt zap electric"),
        ("💥", "boom collision explosion"),
        ("💢", "anger symbol"),
        ("💦", "sweat drops water splash"),
        ("💨", "dash wind fast"),
        ("🎉", "party popper tada celebrate congratulations"),
        ("🎊", "confetti ball"),
        ("🎂", "birthday cake"),
        ("🎁", "gift present"),
        ("🎈", "balloon"),
        ("🏆", "trophy winner champion"),
        ("🥇", "gold medal first"),
        ("🥈", "silver medal second"),
        ("🥉", "bronze medal third"),
        ("🎯", "target bullseye goal"),
        ("🎮", "video game controller"),
        ("🎵", "music note"),
        ("🎶", "music notes"),
        ("🎤", "microphone sing karaoke"),
        ("🎧", "headphones music"),
        ("🎸", "guitar"),
        ("🎬", "clapper movie film"),
        ("📷", "camera photo"),
        ("📱", "phone mobile smartphone"),
        ("💻", "laptop computer"),
        ("🖥️", "desktop computer"),
        ("⌨️", "keyboard"),
        ("🖱️", "mouse computer"),
        ("💾", "floppy disk save"),
        ("📁", "folder file"),
        ("📂", "open folder"),
        ("📄", "page document file"),
        ("📝", "memo note write"),
        ("📌", "pin pushpin"),
        ("📎", "paperclip attachment"),
        ("🔗", "link chain"),
        ("🔒", "lock locked secure"),
        ("🔓", "unlocked open"),
        ("🔑", "key"),
        ("🔔", "bell notification"),
        ("🔕", "bell off mute"),
        ("🔍", "magnifying glass search"),
        ("💡", "light bulb idea"),
        ("🔋", "battery"),
        ("🔌", "plug power"),
        ("⚙️", "gear settings"),
        ("🔧", "wrench tool"),
        ("🔨", "hammer"),
        ("🧰", "toolbox"),
        ("💰", "money bag"),
        ("💵", "dollar cash money"),
        ("💳", "credit card"),
        ("📈", "chart increasing up growth"),
        ("📉", "chart decreasing down"),
        ("📊", "bar chart stats"),
        ("📅", "calendar date"),
        ("⏰", "alarm clock"),
        ("⏳", "hourglass time"),
        ("⌛", "hourglass done"),
        ("📧", "email mail"),
        ("✉️", "envelope mail letter"),
        ("📞", "telephone call"),
        ("📢", "loudspeaker announcement"),
        ("📣", "megaphone cheering"),
        ("📚", "books study"),
        ("📖", "open book read"),
        ("🎓", "graduation cap"),
        ("✏️", "pencil"),
        ("🖊️", "pen"),
        ("✅", "check mark green done yes"),
        ("✔️", "check tick"),
        ("❌", "cross mark no wrong"),
        ("❎", "cross mark button"),
        ("⚠️", "warning caution"),
        ("🚫", "prohibited forbidden no"),
        ("⛔", "no entry stop"),
        ("❓", "question mark red"),
        ("❗", "exclamation red"),
        ("‼️", "double exclamation"),
        ("➕", "plus add"),
        ("➖", "minus subtract"),
        ("➡️", "right arrow"),
        ("⬅️", "left arrow"),
        ("⬆️", "up arrow"),
        ("⬇️", "down arrow"),
        ("🔄", "refresh arrows counterclockwise"),
        ("🔁", "repeat loop"),
        ("▶️", "play button"),
        ("⏸️", "pause button"),
        ("⏹️", "stop button"),
        ("🔊", "speaker loud volume"),
        ("🔇", "muted speaker"),
        ("🐶", "dog puppy"),
        ("🐱", "cat kitten"),
        ("🐭", "mouse animal"),
        ("🐹", "hamster"),
        ("🐰", "rabbit bunny"),
        ("🦊", "fox"),
        ("🐻", "bear"),
        ("🐼", "panda"),
        ("🐨", "koala"),
        ("🐯", "tiger"),
        ("🦁", "lion"),
        ("🐮", "cow"),
        ("🐷", "pig"),
        ("🐸", "frog"),
        ("🐵", "monkey"),
        ("🐔", "chicken"),
        ("🐧", "penguin"),
        ("🐦", "bird"),
        ("🦉", "owl"),
        ("🦄", "unicorn"),
        ("🐝", "bee honeybee"),
        ("🦋", "butterfly"),
        ("🐢", "turtle"),
        ("🐍", "snake python"),
        ("🐙", "octopus"),
        ("🐬", "dolphin"),
        ("🐳", "whale"),
        ("🦈", "shark"),
        ("🐟", "fish"),
        ("🌸", "cherry blossom flower"),
        ("🌹", "rose flower"),
        ("🌻", "sunflower"),
        ("🌷", "tulip"),
        ("🌱", "seedling plant sprout"),
        ("🌲", "evergreen tree"),
        ("🌳", "tree"),
        ("🍀", "four leaf clover luck"),
        ("🌵", "cactus"),
        ("🌞", "sun face"),
        ("☀️", "sun sunny"),
        ("🌙", "crescent moon night"),
        ("⭐", "star"),
        ("☁️", "cloud"),
        ("🌧️", "rain cloud"),
        ("⛈️", "thunderstorm"),
        ("❄️", "snowflake cold winter"),
        ("☃️", "snowman"),
        ("🌈", "rainbow"),
        ("🌊", "wave ocean water"),
        ("🌍", "earth globe europe africa world"),
        ("🌎", "earth globe americas world"),
        ("🍎", "red apple fruit"),
        ("🍌", "banana"),
        ("🍇", "grapes"),
        ("🍓", "strawberry"),
        ("🍉", "watermelon"),
        ("🍑", "peach"),
        ("🍒", "cherries"),
        ("🍋", "lemon"),
        ("🥑", "avocado"),
        ("🍅", "tomato"),
        ("🥕", "carrot"),
        ("🌽", "corn"),
        ("🍕", "pizza"),
        ("🍔", "hamburger burger"),
        ("🍟", "french fries"),
        ("🌭", "hot dog"),
        ("🌮", "taco"),
        ("🌯", "burrito"),
        ("🍣", "sushi"),
        ("🍜", "noodles ramen"),
        ("🍝", "spaghetti pasta"),
        ("🍞", "bread"),
        ("🧀", "cheese"),
        ("🥚", "egg"),
        ("🥓", "bacon"),
        ("🍿", "popcorn"),
        ("🍩", "doughnut donut"),
        ("🍪", "cookie"),
        ("🍫", "chocolate"),
        ("🍦", "ice cream"),
        ("🍰", "cake shortcake"),
        ("☕", "coffee hot drink"),
        ("🍵", "tea"),
        ("🍺", "beer mug"),
        ("🍻", "cheers beers"),
        ("🍷", "wine glass"),
        ("🥂", "champagne cheers toast"),
        ("🍹", "cocktail tropical drink"),
        ("🥤", "cup straw soda"),
        ("🚗", "car automobile"),
        ("🚕", "taxi"),
        ("🚌", "bus"),
        ("🚲", "bicycle bike"),
        ("🏍️", "motorcycle"),
        ("✈️", "airplane plane flight"),
        ("🚀", "rocket launch space ship"),
        ("🚁", "helicopter"),
        ("🚂", "train locomotive"),
        ("⛵", "sailboat"),
        ("🏠", "house home"),
        ("🏢", "office building"),
        ("🏥", "hospital"),
        ("🏫", "school"),
        ("⚽", "soccer football"),
        ("🏀", "basketball"),
        ("🏈", "american football"),
        ("⚾", "baseball"),
        ("🎾", "tennis"),
        ("🏐", "volleyball"),
        ("🎱", "pool billiards"),
        ("🏓", "ping pong table tennis"),
        ("🥊", "boxing glove"),
        ("♟️", "chess pawn"),
        ("🧩", "puzzle piece"),
        ("🎲", "dice game"),
        ("🛒", "shopping cart"),
        ("🛌", "sleeping in bed"),
        ("🚽", "toilet"),
        ("🛁", "bathtub"),
        ("🧼", "soap"),
        ("🧹", "broom clean"),
        ("🧸", "teddy bear"),
        ("👑", "crown king queen"),
        ("💎", "gem diamond"),
        ("💍", "ring wedding"),
        ("👓", "glasses"),
        ("🕶️", "sunglasses"),
        ("👕", "t-shirt shirt"),
        ("👖", "jeans pants"),
        ("👗", "dress"),
        ("👟", "sneaker shoe running"),
        ("🎩", "top hat"),
        ("🧢", "cap hat"),
        ("🇺🇸", "flag united states usa america"),
        ("🇬🇧", "flag united kingdom uk britain"),
        ("🇨🇦", "flag canada"),
        ("🇪🇺", "flag european union europe"),
        ("🇯🇵", "flag japan"),
        ("🇩🇪", "flag germany"),
        ("🇫🇷", "flag france"),
        ("🇲🇽", "flag mexico"),
        ("🏳️‍🌈", "pride rainbow flag"),
        ("🏁", "chequered flag finish race"),
        ("🚩", "red flag"),
        ("♻️", "recycle recycling"),
        ("©️", "copyright"),
        ("™️", "trademark"),
        ("®️", "registered"),
        ("°", "degree symbol"),
        ("→", "arrow right symbol"),
        ("←", "arrow left symbol"),
        ("•", "bullet point dot"),
        ("…", "ellipsis dots"),
        ("—", "em dash"),
        ("×", "multiplication times"),
        ("÷", "division divide"),
        ("≠", "not equal"),
        ("≈", "approximately equal"),
        ("€", "euro currency"),
        ("£", "pound sterling currency"),
        ("¥", "yen yuan currency"),
        ("✓", "check tick symbol"),
        ("¯\\_(ツ)_/¯", "shrug"),
        ("(╯°□°)╯︵ ┻━┻", "table flip rage"),
        ("┬─┬ノ( º _ ºノ)", "put table back calm"),
        ("ಠ_ಠ", "disapproval look"),
        ("( ͡° ͜ʖ ͡°)", "lenny face"),
    ];

    /// The pictures matching what was typed: every typed word must start one of a picture's words;
    /// the ones matching its name (first word) come first. Nothing typed: the first few.
    pub fn search(q: &str) -> Vec<usize> {
        let q = q.trim().to_lowercase();
        if q.is_empty() {
            return (0..LIST.len().min(60)).collect();
        }
        let words: Vec<&str> = q.split_whitespace().collect();
        let mut hits: Vec<(i32, usize)> = Vec::new();
        for (i, (_, keys)) in LIST.iter().enumerate() {
            let kw: Vec<&str> = keys.split_whitespace().collect();
            let mut score = 0;
            let mut ok = true;
            for w in &words {
                match kw.iter().position(|k| k.starts_with(w)) {
                    Some(p) => score += 10 - p.min(9) as i32 + if kw[p] == *w { 5 } else { 0 },
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                hits.push((score, i));
            }
        }
        hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        hits.dedup_by_key(|h| LIST[h.1].0);
        hits.into_iter().take(60).map(|h| h.1).collect()
    }
}

mod logic {
    /// A new word starts here: the first letter, a letter after a separator, or a capital after a small letter.
    pub fn is_word_start(n: &[u8], i: usize) -> bool {
        if i == 0 {
            return true;
        }
        let (p, c) = (n[i - 1], n[i]);
        (p < 0x80 && !p.is_ascii_alphanumeric()) || (p.is_ascii_lowercase() && c.is_ascii_uppercase())
    }

    /// Like `fuzzy`, and when that finds nothing, a name with a slip in it ("chorme" for Chrome).
    pub fn matched(name: &[u8], q: &[u8], loose: bool, pos: &mut Vec<usize>) -> Option<i32> {
        fuzzy(name, q, loose, pos).or_else(|| if q.len() >= 4 { typo(name, q, pos) } else { None })
    }

    /// Letters wrong, missing, extra or swapped: the fewest changes to turn `a` into `b` (swapping two
    /// neighbours counts once). Only for short words.
    pub fn distance(a: &[u8], b: &[u8]) -> usize {
        const MAX: usize = 48;
        let (n, m) = (a.len(), b.len());
        if n >= MAX || m >= MAX {
            return distance_big(a, b);
        }
        // short words (all it is used for): the table lives on the stack
        let mut d = [[0u8; MAX + 1]; MAX + 1];
        for (i, row) in d.iter_mut().enumerate().take(n + 1) {
            row[0] = i as u8;
        }
        for j in 0..=m {
            d[0][j] = j as u8;
        }
        for i in 1..=n {
            for j in 1..=m {
                let cost = u8::from(a[i - 1] != b[j - 1]);
                let mut v = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
                if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                    v = v.min(d[i - 2][j - 2] + 1);
                }
                d[i][j] = v;
            }
        }
        d[n][m] as usize
    }

    /// `distance` for long words.
    pub fn distance_big(a: &[u8], b: &[u8]) -> usize {
        let (n, m) = (a.len(), b.len());
        let mut d = vec![vec![0usize; m + 1]; n + 1];
        for (i, row) in d.iter_mut().enumerate() {
            row[0] = i;
        }
        for j in 0..=m {
            d[0][j] = j;
        }
        for i in 1..=n {
            for j in 1..=m {
                let cost = usize::from(a[i - 1] != b[j - 1]);
                let mut v = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
                if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                    v = v.min(d[i - 2][j - 2] + 1);
                }
                d[i][j] = v;
            }
        }
        d[n][m]
    }

    /// The query is close to the start of one of the words of `name`: one slip for 4 - 5 letters,
    /// two for longer ones. The word is left in `pos`.
    pub fn typo(name: &[u8], q: &[u8], pos: &mut Vec<usize>) -> Option<i32> {
        pos.clear();
        let (n, m) = (name.len(), q.len());
        if !(4..=40).contains(&m) || n < m - 1 {
            return None;
        }
        let allowed = if m <= 5 { 1 } else { 2 };
        let low = |i: usize| name[i].to_ascii_lowercase();
        let mut best: Option<(usize, usize, usize)> = None; // (distance, start, length)
        for st in 0..n {
            if !is_word_start(name, st) || !name[st].is_ascii_alphanumeric() {
                continue;
            }
            // a cheap look at the first letters, so most words are skipped
            let swapped = st + 1 < n && m >= 2 && low(st) == q[1] && low(st + 1) == q[0];
            if low(st) != q[0] && !swapped {
                continue;
            }
            let mut end = st;
            while end < n && name[end].is_ascii_alphanumeric() && (end == st || !is_word_start(name, end)) {
                end += 1;
            }
            let wlen = end - st;
            if wlen + allowed < m {
                continue;
            }
            // the start of the word (lower case, on the stack), about as long as the query
            let mut word = [0u8; 48];
            let take = wlen.min(m + 1).min(48);
            for (w, c) in word.iter_mut().zip(&name[st..st + take]) {
                *w = c.to_ascii_lowercase();
            }
            // the start of the word, about as long as the query
            for len in m.saturating_sub(1)..=(m + 1) {
                let len = len.min(wlen).min(take);
                let d = distance(&word[..len], q);
                if d <= allowed && best.map_or(true, |b| d < b.0) {
                    best = Some((d, st, len));
                }
            }
        }
        let (d, st, len) = best?;
        pos.extend(st..st + len);
        Some(3_400 - 500 * d as i32 - st as i32)
    }

    /// How well `name` fits the lowercase query `q` (None = not at all). The matched byte positions
    /// are left in `pos`. `loose` also accepts letters spread over the name ("vsc" for Visual Studio Code).
    pub fn fuzzy(name: &[u8], q: &[u8], loose: bool, pos: &mut Vec<usize>) -> Option<i32> {
        pos.clear();
        let (n, m) = (name.len(), q.len());
        if m == 0 || m > n {
            return None;
        }
        let eq = |a: u8, b: u8| a.to_ascii_lowercase() == b;
        // the query as one piece, preferably at the start of a word
        let mut first = None;
        let mut at_word = None;
        for i in 0..=n - m {
            if (0..m).all(|j| eq(name[i + j], q[j])) {
                if first.is_none() {
                    first = Some(i);
                }
                if is_word_start(name, i) {
                    at_word = Some(i);
                    break;
                }
            }
        }
        if let Some(i) = at_word.or(first) {
            pos.extend(i..i + m);
            return Some(if i == 0 {
                if m == n {
                    10_000
                } else {
                    8_000 - (n - m) as i32
                }
            } else if is_word_start(name, i) {
                6_000 - i as i32
            } else {
                4_000 - i as i32
            });
        }
        // the first letters of the words ("vsc")
        let mut k = 0;
        for i in 0..n {
            if is_word_start(name, i) {
                if eq(name[i], q[k]) {
                    pos.push(i);
                    k += 1;
                    if k == m {
                        return Some(5_000 - n as i32);
                    }
                } else {
                    break;
                }
            }
        }
        pos.clear();
        if !loose {
            return None;
        }
        // letters in order, not too far apart
        let mut k = 0;
        let mut score = 1_000i32;
        let mut last: Option<usize> = None;
        for i in 0..n {
            if eq(name[i], q[k]) {
                pos.push(i);
                if last.is_some_and(|l| l + 1 == i) {
                    score += 40;
                }
                if is_word_start(name, i) {
                    score += 60;
                }
                last = Some(i);
                k += 1;
                if k == m {
                    break;
                }
            }
        }
        if k < m {
            pos.clear();
            return None;
        }
        let span = pos[m - 1] - pos[0] + 1;
        if span > m * 3 + 4 {
            pos.clear();
            return None;
        }
        Some((score - (span - m) as i32 * 10 - pos[0] as i32).min(3_900))
    }

    /// Byte positions as ranges.
    pub fn spans(pos: &[usize]) -> Vec<(usize, usize)> {
        let mut out: Vec<(usize, usize)> = Vec::new();
        for &p in pos {
            match out.last_mut() {
                Some(l) if l.1 == p => l.1 = p + 1,
                _ => out.push((p, p + 1)),
            }
        }
        out
    }

    /// A range moved onto character boundaries.
    pub fn snap(s: &str, a: usize, b: usize) -> (usize, usize) {
        let (mut a, mut b) = (a.min(s.len()), b.min(s.len()));
        while a > 0 && !s.is_char_boundary(a) {
            a -= 1;
        }
        while b < s.len() && !s.is_char_boundary(b) {
            b += 1;
        }
        (a, b)
    }

    /// Long text cut in the middle: `C:\Users\me\Doc…\notes\todo.txt`.
    pub fn mid(s: &str, max: usize) -> String {
        let n = s.chars().count();
        if n <= max || max < 8 {
            return s.to_string();
        }
        let head = (max - 1) * 2 / 5;
        let tail = max - 1 - head;
        let a: String = s.chars().take(head).collect();
        let b: String = s.chars().skip(n - tail).collect();
        format!("{a}…{b}")
    }

    // -- calculator ------------------------------------------------------------------------------------

    struct P {
        s: Vec<char>,
        i: usize,
        ops: u32,
    }

    impl P {
        fn ws(&mut self) {
            while self.s.get(self.i).is_some_and(|c| c.is_whitespace()) {
                self.i += 1;
            }
        }
        fn peek(&mut self) -> Option<char> {
            self.ws();
            self.s.get(self.i).copied()
        }
        fn expr(&mut self) -> Option<f64> {
            let mut v = self.term()?;
            while let Some(c) = self.peek() {
                if c != '+' && c != '-' {
                    break;
                }
                self.i += 1;
                self.ops += 1;
                let r = self.term()?;
                v = if c == '+' { v + r } else { v - r };
            }
            Some(v)
        }
        fn term(&mut self) -> Option<f64> {
            let mut v = self.factor()?;
            while let Some(c) = self.peek() {
                if !matches!(c, '*' | '/' | '%' | '×' | '÷') {
                    break;
                }
                self.i += 1;
                self.ops += 1;
                let r = self.factor()?;
                v = match c {
                    '*' | '×' => v * r,
                    '/' | '÷' => v / r,
                    _ => v % r,
                };
            }
            Some(v)
        }
        fn factor(&mut self) -> Option<f64> {
            match self.peek()? {
                '-' => {
                    self.i += 1;
                    Some(-self.factor()?)
                }
                '+' => {
                    self.i += 1;
                    self.factor()
                }
                _ => {
                    let b = self.atom()?;
                    if self.peek() == Some('^') {
                        self.i += 1;
                        self.ops += 1;
                        let e = self.factor()?;
                        Some(b.powf(e))
                    } else {
                        Some(b)
                    }
                }
            }
        }
        fn atom(&mut self) -> Option<f64> {
            let c = self.peek()?;
            if c == '(' {
                self.i += 1;
                self.ops += 1;
                let v = self.expr()?;
                if self.peek()? != ')' {
                    return None;
                }
                self.i += 1;
                return Some(v);
            }
            if c.is_ascii_digit() || c == '.' {
                let st = self.i;
                while self.s.get(self.i).is_some_and(|c| c.is_ascii_digit() || *c == '.') {
                    self.i += 1;
                }
                let t: String = self.s[st..self.i].iter().collect();
                return t.parse::<f64>().ok();
            }
            if c.is_ascii_alphabetic() {
                let st = self.i;
                while self.s.get(self.i).is_some_and(|c| c.is_ascii_alphabetic()) {
                    self.i += 1;
                }
                let name: String = self.s[st..self.i].iter().collect::<String>().to_ascii_lowercase();
                if self.peek() == Some('(') {
                    self.i += 1;
                    self.ops += 1;
                    let v = self.expr()?;
                    if self.peek()? != ')' {
                        return None;
                    }
                    self.i += 1;
                    return match name.as_str() {
                        "sqrt" => Some(v.sqrt()),
                        "sin" => Some(v.sin()),
                        "cos" => Some(v.cos()),
                        "tan" => Some(v.tan()),
                        "ln" => Some(v.ln()),
                        "log" => Some(v.log10()),
                        "abs" => Some(v.abs()),
                        "round" => Some(v.round()),
                        "floor" => Some(v.floor()),
                        "ceil" => Some(v.ceil()),
                        _ => None,
                    };
                }
                return match name.as_str() {
                    "pi" => Some(std::f64::consts::PI),
                    "e" => Some(std::f64::consts::E),
                    _ => None,
                };
            }
            None
        }
    }

    /// The answer when `src` is a sum like `12*(3+4)` or `sqrt(2)`; a lone number is not one.
    pub fn calc(src: &str) -> Option<String> {
        let times = times(src.trim());
        let s = times.as_str();
        if s.len() > 120 || !s.bytes().any(|b| b.is_ascii_digit() || b == b'p' || b == b'P') {
            return None;
        }
        let mut p = P { s: s.chars().collect(), i: 0, ops: 0 };
        let v = p.expr()?;
        p.ws();
        if p.i != p.s.len() || p.ops == 0 || !v.is_finite() {
            return None;
        }
        Some(num(v))
    }

    /// `6x7`, `6 x 7` and `6×7` mean 6*7 (and `÷` means `/`); the x of a word or of `0x1F` stays.
    pub fn times(s: &str) -> String {
        let c: Vec<char> = s.chars().collect();
        let mut out = String::with_capacity(s.len());
        for (i, &ch) in c.iter().enumerate() {
            match ch {
                '×' => out.push('*'),
                '÷' => out.push('/'),
                'x' | 'X' => {
                    let prev = c[..i].iter().rev().find(|k| !k.is_whitespace());
                    let next = c[i + 1..].iter().find(|k| !k.is_whitespace());
                    let ok = matches!(prev, Some(k) if k.is_ascii_digit() || *k == ')' || *k == '.')
                        && matches!(next, Some(k) if k.is_ascii_digit() || *k == '(' || *k == '.');
                    let hex = i >= 1
                        && c[i - 1] == '0'
                        && (i < 2 || !(c[i - 2].is_ascii_digit() || c[i - 2] == '.'))
                        && matches!(next, Some(k) if k.is_ascii_hexdigit());
                    out.push(if ok && !hex { '*' } else { ch });
                }
                _ => out.push(ch),
            }
        }
        out
    }

    /// "just now", "5m ago", "3h ago", "2d ago".
    pub fn ago(secs: u64) -> String {
        match secs {
            0..=9 => "just now".to_string(),
            10..=59 => format!("{secs}s ago"),
            60..=3599 => format!("{}m ago", secs / 60),
            3600..=86399 => format!("{}h ago", secs / 3600),
            _ => format!("{}d ago", secs / 86400),
        }
    }

    pub fn num(v: f64) -> String {
        if v == v.trunc() && v.abs() < 1e15 {
            return format!("{}", v as i64);
        }
        if v.abs() >= 1e15 || v.abs() < 1e-6 {
            return format!("{v:e}");
        }
        let t = format!("{v:.10}");
        t.trim_end_matches('0').trim_end_matches('.').to_string()
    }

    // -- links -----------------------------------------------------------------------------------------

    /// `https://…`, `www.…` or a bare `name.com` becomes an address to open.
    pub fn as_url(q: &str) -> Option<String> {
        let q = q.trim();
        if q.is_empty() || q.contains(char::is_whitespace) || q.len() > 300 {
            return None;
        }
        let l = q.to_ascii_lowercase();
        if l.starts_with("http://") || l.starts_with("https://") {
            return (q.len() > 8).then(|| q.to_string());
        }
        if l.starts_with("www.") {
            return (q.len() > 6).then(|| format!("https://{q}"));
        }
        let host = l.split(['/', '?', '#']).next().unwrap_or("");
        let (name, tld) = host.rsplit_once('.')?;
        const TLDS: [&str; 24] = [
            "com", "org", "net", "io", "dev", "app", "ai", "co", "edu", "gov", "uk", "de", "fr", "me", "tv", "info",
            "us", "ca", "au", "nl", "es", "it", "xyz", "gg",
        ];
        let name_ok = !name.is_empty()
            && name.split('.').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
        (name_ok && TLDS.contains(&tld)).then(|| format!("https://{q}"))
    }

    /// Text made safe for a web address.
    pub fn enc(q: &str) -> String {
        let mut out = String::with_capacity(q.len() * 3);
        for b in q.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
                b' ' => out.push('+'),
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }

    // -- weather words ---------------------------------------------------------------------------------

    /// "wea", "weather", "cast", "forecast", "temp", "temperature" ...: the user wants the weather.
    pub fn weather_query(q: &str) -> bool {
        let l = q.trim().to_ascii_lowercase();
        let n = l.len();
        (n >= 3 && "weather".starts_with(l.as_str()))
            || (n >= 4 && "forecast".contains(l.as_str()))
            || (n >= 4 && "temperature".starts_with(l.as_str()))
    }

    // -- easing ----------------------------------------------------------------------------------------

    /// Fast start, soft landing.
    pub fn ease_out(t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t)
    }

    /// Like `ease_out`, but swings a little past 1 before settling.
    pub fn ease_spring(t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0) - 1.0;
        1.0 + 4.0 * t * t * t + 3.0 * t * t
    }

    #[allow(dead_code)]
    pub fn ease_back(t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0) - 1.0;
        1.0 + 2.70158 * t * t * t + 1.70158 * t * t
    }

    /// `v` a step closer to `to`, without passing it.
    pub fn toward(v: f32, to: f32, step: f32) -> f32 {
        if (to - v).abs() <= step {
            to
        } else if to > v {
            v + step
        } else {
            v - step
        }
    }

    /// `v` moves a fraction of the way to `to` that grows with the time passed (smooth, never overshoots).
    pub fn follow(v: f32, to: f32, dt: f32, rate: f32, eps: f32) -> f32 {
        let d = to - v;
        if d.abs() <= eps {
            to
        } else {
            v + d * (1.0 - (-rate * dt).exp())
        }
    }

    // -- the blurred picture behind the window ----------------------------------------------------------

    /// One pass of a box blur along a row or a column of an interleaved BGRA picture (the first three
    /// channels; the edge pixels repeat).
    fn blur_line(src: &[u8], dst: &mut [u8], start: usize, step: usize, len: usize, r: usize) {
        let n = (2 * r + 1) as u32;
        let last = len as isize - 1;
        for c in 0..3 {
            let at = |i: isize| -> u32 { src[start + i.clamp(0, last) as usize * step + c] as u32 };
            let mut sum = 0u32;
            for i in -(r as isize)..=(r as isize) {
                sum += at(i);
            }
            for i in 0..len {
                dst[start + i * step + c] = ((sum + n / 2) / n) as u8;
                sum = sum + at(i as isize + r as isize + 1) - at(i as isize - r as isize);
            }
        }
    }

    /// A screen copy (`w` x `h`, BGRA, no alpha) shrunk `k` times by averaging and blurred with three box
    /// passes of radius `r`. Returns premultiplied BGRA bytes (opaque) and the new size.
    pub fn shrink_blur(src: &[u32], w: usize, h: usize, k: usize, r: usize) -> (Vec<u8>, usize, usize) {
        let k = k.max(1);
        let (w2, h2) = (w.div_ceil(k).max(1), h.div_ceil(k).max(1));
        let mut a = vec![255u8; w2 * h2 * 4];
        if src.len() >= w * h {
            for y in 0..h2 {
                for x in 0..w2 {
                    let (mut b, mut g, mut rr, mut cnt) = (0u32, 0u32, 0u32, 0u32);
                    for yy in (y * k)..((y + 1) * k).min(h) {
                        for xx in (x * k)..((x + 1) * k).min(w) {
                            let p = src[yy * w + xx];
                            b += p & 255;
                            g += (p >> 8) & 255;
                            rr += (p >> 16) & 255;
                            cnt += 1;
                        }
                    }
                    let cnt = cnt.max(1);
                    let o = (y * w2 + x) * 4;
                    a[o] = (b / cnt) as u8;
                    a[o + 1] = (g / cnt) as u8;
                    a[o + 2] = (rr / cnt) as u8;
                }
            }
        }
        if r > 0 {
            let mut b = a.clone();
            for _ in 0..3 {
                for y in 0..h2 {
                    blur_line(&a, &mut b, y * w2 * 4, 4, w2, r);
                }
                for x in 0..w2 {
                    blur_line(&b, &mut a, x * 4, w2 * 4, h2, r);
                }
            }
        }
        (a, w2, h2)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn f(name: &str, q: &str, loose: bool) -> Option<(i32, Vec<(usize, usize)>)> {
            let mut pos = Vec::new();
            let q = q.to_ascii_lowercase();
            fuzzy(name.as_bytes(), q.as_bytes(), loose, &mut pos).map(|s| (s, spans(&pos)))
        }

        #[test]
        fn ranks_sensibly() {
            let exact = f("Chrome", "chrome", true).unwrap().0;
            let prefix = f("Chrome Remote Desktop", "chrome", true).unwrap().0;
            let word = f("Google Chrome", "chrome", true).unwrap().0;
            let inner = f("Archrome", "chrome", true).unwrap().0;
            assert!(exact > prefix && prefix > word && word > inner, "{exact} {prefix} {word} {inner}");
            assert_eq!(f("bees.gif", "be", false).unwrap().1, vec![(0, 2)]);
            assert_eq!(f("Visual Studio Code", "vsc", false).unwrap().1, vec![(0, 1), (7, 8), (14, 15)]);
            assert!(f("Visual Studio Code", "vcode", false).is_some() == false);
            assert!(f("Visual Studio Code", "vstc", true).is_some());
            assert!(f("notes.txt", "xyz", true).is_none());
            assert!(f("a", "ab", true).is_none());
            assert_eq!(f("MyCoolApp", "cool", false).unwrap().1, vec![(2, 6)]);
        }

        #[test]
        fn slips_are_forgiven() {
            let mut pos = Vec::new();
            assert!(typo(b"Google Chrome", b"chorme", &mut pos).is_some());
            assert_eq!(pos, (7..13).collect::<Vec<_>>());
            assert!(typo(b"Notepad", b"nodepad", &mut pos).is_some());
            assert!(typo(b"Visual Studio Code", b"vsiual", &mut pos).is_some());
            assert!(typo(b"Firefox", b"chrome", &mut pos).is_none());
            assert!(typo(b"Calculator", b"chrome", &mut pos).is_none());
            assert!(typo(b"Chrome", b"chr", &mut pos).is_none());
            assert!(matched(b"Google Chrome", b"chorme", true, &mut pos).is_some());
            // a real match still wins over a slip
            let real = fuzzy(b"Chrome", b"chrome", true, &mut pos).unwrap();
            let slip = typo(b"Chrome", b"chorme", &mut pos).unwrap();
            assert!(real > slip);
            assert_eq!(distance(b"chrome", b"chorme"), 1);
            assert_eq!(distance(b"abc", b"abcd"), 1);
        }

        #[test]
        fn ages() {
            assert_eq!(ago(3), "just now");
            assert_eq!(ago(45), "45s ago");
            assert_eq!(ago(300), "5m ago");
            assert_eq!(ago(7300), "2h ago");
            assert_eq!(ago(200_000), "2d ago");
        }

        #[test]
        fn times_sign() {
            assert_eq!(calc("6x7").as_deref(), Some("42"));
            assert_eq!(calc("6 X 7").as_deref(), Some("42"));
            assert_eq!(calc("6×7").as_deref(), Some("42"));
            assert_eq!(calc("2x(3+4)").as_deref(), Some("14"));
            assert_eq!(calc("(1+1)x3").as_deref(), Some("6"));
            assert_eq!(calc("12÷4").as_deref(), Some("3"));
            assert_eq!(calc("6*7").as_deref(), Some("42"));
            assert_eq!(times("max(2,3)"), "max(2,3)");
            assert_eq!(times("0x1F"), "0x1F");
            assert_eq!(calc("xbox"), None);
        }

        #[test]
        fn loose_is_not_too_loose() {
            assert!(f("a very long file name with many letters inside it.txt", "avz", true).is_none());
            assert!(f("report-final-v2.docx", "rfv", true).is_some());
        }

        #[test]
        fn calculates() {
            assert_eq!(calc("2+2").as_deref(), Some("4"));
            assert_eq!(calc("12*(3+4)").as_deref(), Some("84"));
            assert_eq!(calc("2^10").as_deref(), Some("1024"));
            assert_eq!(calc("-2^2").as_deref(), Some("-4"));
            assert_eq!(calc("2^-1").as_deref(), Some("0.5"));
            assert_eq!(calc("10/4").as_deref(), Some("2.5"));
            assert_eq!(calc("1/3").as_deref(), Some("0.3333333333"));
            assert_eq!(calc("sqrt(16)+1").as_deref(), Some("5"));
            assert_eq!(calc("2*pi").as_deref(), Some("6.2831853072"));
            assert_eq!(calc("7 % 4").as_deref(), Some("3"));
            assert_eq!(calc("3 × 4").as_deref(), Some("12"));
            assert_eq!(calc("42"), None);
            assert_eq!(calc("1/0"), None);
            assert_eq!(calc("2+"), None);
            assert_eq!(calc("(2+3"), None);
            assert_eq!(calc("chrome"), None);
            assert_eq!(calc("notes2+x"), None);
            assert_eq!(calc("1e5"), None);
            assert_eq!(calc("0.1+0.2").as_deref(), Some("0.3"));
        }

        #[test]
        fn links() {
            assert_eq!(as_url("https://a.b/c").as_deref(), Some("https://a.b/c"));
            assert_eq!(as_url("www.rust-lang.org").as_deref(), Some("https://www.rust-lang.org"));
            assert_eq!(as_url("github.com/foo/bar").as_deref(), Some("https://github.com/foo/bar"));
            assert_eq!(as_url("notes.txt"), None);
            assert_eq!(as_url("hello world.com"), None);
            assert_eq!(as_url("chrome"), None);
            assert_eq!(enc("a b&c/é"), "a+b%26c%2F%C3%A9");
        }

        #[test]
        fn text_helpers() {
            assert_eq!(snap("héllo", 2, 2), (1, 3));
            assert_eq!(mid("short", 20), "short");
            let m = mid(r"C:\Users\someone\Documents\Projects\notes\todo.txt", 30);
            assert_eq!(m.chars().count(), 30);
            assert!(m.starts_with(r"C:\Users\s") && m.ends_with("todo.txt"));
        }
        #[test]
        fn weather_words() {
            for q in ["cast", "wea", "weather", "Forecast", "temp", "temperature", "fore"] {
                assert!(weather_query(q), "{q}");
            }
            for q in ["ca", "we", "weathers", "fort", "tem", "chrome", ""] {
                assert!(!weather_query(q), "{q}");
            }
        }

        #[test]
        fn easing_and_moving() {
            assert_eq!(ease_out(0.0), 0.0);
            assert_eq!(ease_out(1.0), 1.0);
            assert!(ease_out(0.5) > 0.8);
            assert!((ease_back(1.0) - 1.0).abs() < 1e-6);
            assert!(ease_back(0.8) > 1.0, "swings past 1");
            assert_eq!(toward(0.0, 1.0, 0.3), 0.3);
            assert_eq!(toward(0.9, 1.0, 0.3), 1.0);
            assert_eq!(toward(1.0, 0.0, 0.25), 0.75);
            assert_eq!(follow(0.0, 10.0, 1.0, 1e9, 0.1), 10.0);
            assert_eq!(follow(9.95, 10.0, 0.01, 5.0, 0.1), 10.0);
            let v = follow(0.0, 10.0, 0.016, 16.0, 0.01);
            assert!(v > 0.0 && v < 10.0);
        }

        #[test]
        fn blur_keeps_flat_pictures_and_spreads_points() {
            let flat = vec![0x00_40_80_C0u32; 20 * 12];
            let (px, w, h) = shrink_blur(&flat, 20, 12, 4, 2);
            assert_eq!((w, h), (5, 3));
            assert!(px.chunks(4).all(|c| c == [0xC0, 0x80, 0x40, 255]), "a flat picture stays flat");
            let mut one = vec![0u32; 16 * 16];
            one[8 * 16 + 8] = 0x00_FF_FF_FF;
            let (sharp, ..) = shrink_blur(&one, 16, 16, 1, 0);
            let (soft, ..) = shrink_blur(&one, 16, 16, 1, 2);
            assert_eq!(sharp[(8 * 16 + 8) * 4], 255);
            assert!(soft[(8 * 16 + 8) * 4] < 60 && soft[(8 * 16 + 9) * 4] > 0, "the point spreads out");
            assert!(soft.chunks(4).all(|c| c[3] == 255));
            let (odd, w, h) = shrink_blur(&flat[..7 * 5], 7, 5, 4, 1);
            assert_eq!((w, h, odd.len()), (2, 2, 16));
        }
    }
}
