//! The Alcove launcher: the quick launcher, drawn inside the notch.
//!
//! It is a hidden page of the notch. It has no tab and cannot be scrolled to: only the launcher hotkeys
//! open it (the launcher, clipboard, Ask AI, Run and window-switcher hotkeys), and only while the Alcove look
//! is on and `[launcher] alcove` is true. Otherwise the hotkeys open Tuna's launcher as before.
//!
//! It uses the helpers of `launcher.rs` (the calculator, conversions, link and fuzzy matching, the Run box
//! history, quitting programs) through the `bridge_*` functions there, so both launchers find the same things
//! the same way. Modes (Tab cycles them, or click a chip): Search (apps, files, sums, conversions, links, the
//! web, computer commands), Clipboard, Ask AI (hands the question to the Gemini page), Quit apps, Run, and the
//! window switcher.
//!
//! Cost: nothing is built until the first hotkey press. The app and file lists are looked for in the
//! background then, and handed back 30 seconds after the launcher closes. Frames only come while something
//! moves (opening, the selection gliding, rows sliding in).

use std::any::Any;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_MENU};
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

use super::widgets::TextField;
use super::{Cx, Effects, Key, Module, ModuleId, SystemEvent};
use crate::anim::{ease_out, smoothstep};
use crate::bus::Bus;
use crate::gfx::{Icon, ImageData, TextStyle};
use crate::sys::{self, clip};
use crate::ui::{gkey, Ui};
use crate::util::{palette, Color, Rect, SendHwnd};

pub const ID: ModuleId = "alcove-launcher";

/// One row of a list.
const ROW: f32 = 46.0;
/// Rows that show at once.
const VIS: usize = 6;
/// The line with the icon and the text field.
const HEAD: f32 = 46.0;
/// The line of mode chips.
const CHIPS: f32 = 20.0;
const GAP: f32 = 8.0;
/// A window card of the switcher.
const CARD_W: f32 = 150.0;
const CARD_H: f32 = 104.0;
const CARD_GAP: f32 = 10.0;
/// Matches kept for a search.
const KEEP: usize = 30;
/// Commands the Run box remembers.
const RUN_KEEP: usize = 20;
/// How long the lists are kept after the launcher closed.
const FREE_AFTER: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Search,
    Clip,
    Ai,
    Kill,
    Run,
    Windows,
}

const MODES: [Mode; 6] = [Mode::Search, Mode::Clip, Mode::Ai, Mode::Kill, Mode::Run, Mode::Windows];

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Search => "Search",
            Mode::Clip => "Clipboard",
            Mode::Ai => "Ask AI",
            Mode::Kill => "Quit apps",
            Mode::Run => "Run",
            Mode::Windows => "Windows",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Mode::Search => Icon::Grid,
            Mode::Clip => Icon::Clipboard,
            Mode::Ai => Icon::Sparkle,
            Mode::Kill => Icon::Cpu,
            Mode::Run => Icon::Keyboard,
            Mode::Windows => Icon::Image,
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Mode::Search => "Search apps, files, sums or the web",
            Mode::Clip => "Search what you copied",
            Mode::Ai => "Ask Gemini anything",
            Mode::Kill => "Pick an app to quit",
            Mode::Run => "Type a command to run",
            Mode::Windows => "Switch to a window",
        }
    }

    fn next(self) -> Mode {
        let i = MODES.iter().position(|m| *m == self).unwrap_or(0);
        MODES[(i + 1) % MODES.len()]
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum K {
    App,
    Folder,
    File,
    Calc,
    Url,
    Web,
    System,
    Clip,
    Prog,
    Cmd,
}

/// A thing that can be chosen.
#[derive(Clone)]
struct Hit {
    kind: K,
    name: String,
    detail: String,
    /// What gets opened (and the key of its icon).
    path: String,
    /// Place in the system table (System), kind of item (Clip).
    idx: usize,
    /// The clipboard item, or 1 when a system command asks twice.
    id: u64,
    thumb: Option<ImageData>,
}

impl Hit {
    fn new(kind: K, name: String, detail: String, path: String) -> Self {
        Self { kind, name, detail, path, idx: 0, id: 0, thumb: None }
    }
}

/// From the background threads.
enum Msg {
    Apps(Vec<(String, String)>),
    Files(Vec<(String, bool)>),
}

/// The shell icons shown so far. At most three new ones are loaded per frame so a long list never stalls.
struct Icons {
    map: HashMap<String, Option<ImageData>>,
    budget: u8,
    starved: bool,
}

impl Icons {
    fn get(&mut self, path: &str, name: &str) -> Option<ImageData> {
        if path.is_empty() {
            return None;
        }
        if let Some(v) = self.map.get(path) {
            return v.clone();
        }
        if self.budget == 0 {
            self.starved = true;
            return None;
        }
        self.budget -= 1;
        let img = crate::dock::icon_for_path(path, name);
        if self.map.len() > 200 {
            self.map.clear();
        }
        self.map.insert(path.to_string(), img.clone());
        img
    }
}

pub struct AlcoveLauncher {
    input: TextField,
    mode: Mode,
    /// Shown right now (from the hotkey until the notch closes).
    open: bool,
    hits: Vec<Hit>,
    sel: usize,
    /// First row of the list that shows.
    top: usize,
    apps: Vec<(String, String)>,
    files: Vec<(String, bool)>,
    apps_started: bool,
    files_started: bool,
    /// What was opened before (this run): the apps used most come first.
    usage: HashMap<String, u32>,
    icons: Icons,
    /// The open windows (the switcher and the quit list).
    wins: Vec<crate::switcher::Win>,
    wsel: usize,
    run_hist: Vec<String>,
    /// A system command that waits for its second Enter (its place in the table).
    armed: Option<usize>,
    notice: Option<(String, Instant, bool)>,
    t_list: Instant,
    t_mode: Instant,
    closed_at: Option<Instant>,
    /// The window that had the focus before the hotkey, and whether it gets it back on closing.
    prev_fg: isize,
    restore_focus: bool,
    /// The switcher was opened with Alt held: letting go of Alt chooses.
    alt_open: bool,
    hwnd: SendHwnd,
    search_url: String,
    files_on: bool,
    folders: Vec<String>,
}

impl AlcoveLauncher {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            input: TextField::new(300).flat(),
            mode: Mode::Search,
            open: false,
            hits: Vec::new(),
            sel: 0,
            top: 0,
            apps: Vec::new(),
            files: Vec::new(),
            apps_started: false,
            files_started: false,
            usage: HashMap::new(),
            icons: Icons { map: HashMap::new(), budget: 3, starved: false },
            wins: Vec::new(),
            wsel: 0,
            run_hist: Vec::new(),
            armed: None,
            notice: None,
            t_list: now,
            t_mode: now,
            closed_at: None,
            prev_fg: 0,
            restore_focus: true,
            alt_open: false,
            hwnd: SendHwnd(0),
            search_url: "https://www.google.com/search?q=".to_string(),
            files_on: true,
            folders: Vec::new(),
        }
    }

    fn holds_memory(&self) -> bool {
        self.apps_started || self.files_started || !self.icons.map.is_empty()
    }

    /// Hand the lists back (the launcher has been closed for a while).
    fn free(&mut self) {
        self.apps = Vec::new();
        self.files = Vec::new();
        self.apps_started = false;
        self.files_started = false;
        self.icons.map = HashMap::new();
        self.wins = Vec::new();
        self.closed_at = None;
    }

    /// The apps (and files) are looked for in the background the first time they are needed.
    fn ensure_index(&mut self, bus: &Bus) {
        if !self.apps_started {
            self.apps_started = true;
            let bus = bus.clone();
            let _ = std::thread::Builder::new().name("alcove-launcher-apps".into()).spawn(move || {
                let list = crate::launcher::bridge_apps();
                bus.to_module(ID, Msg::Apps(list));
            });
        }
        if self.files_on && !self.files_started {
            self.files_started = true;
            let bus = bus.clone();
            let folders = self.folders.clone();
            let _ = std::thread::Builder::new().name("alcove-launcher-files".into()).spawn(move || {
                let list = crate::launcher::bridge_files(&folders);
                bus.to_module(ID, Msg::Files(list));
            });
        }
    }

    // -- opening, closing, modes ------------------------------------------------------------------------

    /// A launcher hotkey was pressed. `what` is which one; `showing` is whether this page is on screen already.
    fn launch(&mut self, what: &str, showing: bool, cx: &mut Cx) {
        let (mode, back) = match what {
            "clip" => (Mode::Clip, false),
            "ai" => (Mode::Ai, false),
            "run" => (Mode::Run, false),
            "switch" => (Mode::Windows, false),
            "switch-back" => (Mode::Windows, true),
            _ => (Mode::Search, false),
        };
        if showing && self.open && self.mode == mode {
            if mode == Mode::Windows {
                // the switcher key again: on to the next window
                self.step(if back { -1 } else { 1 });
                cx.fx.redraw = true;
            } else {
                // the same hotkey again puts it away
                cx.fx.collapse = true;
            }
            return;
        }
        if !showing || !self.open {
            let fg = unsafe { GetForegroundWindow() };
            let fg = fg.0 as isize;
            self.prev_fg = if fg == self.hwnd.0 { 0 } else { fg };
            self.restore_focus = true;
        }
        self.open = true;
        self.closed_at = None;
        self.t_list = Instant::now();
        self.set_mode(mode, cx.bus);
        self.alt_open = mode == Mode::Windows && alt_down();
        if mode == Mode::Windows && back {
            self.step(-1);
            self.step(-1);
        }
        cx.fx.open_page = Some(ID);
        cx.fx.keyboard_focus = Some(true);
        cx.fx.redraw = true;
    }

    /// The notch closed.
    fn closed(&mut self) {
        if !self.open {
            return;
        }
        self.open = false;
        self.alt_open = false;
        // the focus goes back to what had it, unless something else was started or chosen meanwhile
        if self.restore_focus && self.prev_fg != 0 {
            let now = unsafe { GetForegroundWindow() };
            if now.0 as isize == self.hwnd.0 {
                sys::activate_window(self.prev_fg);
            }
        }
        self.prev_fg = 0;
        self.input.clear();
        self.hits = Vec::new();
        self.wins = Vec::new();
        self.armed = None;
        self.notice = None;
        self.closed_at = Some(Instant::now());
    }

    fn set_mode(&mut self, mode: Mode, bus: &Bus) {
        self.mode = mode;
        self.t_mode = Instant::now();
        self.t_list = Instant::now();
        self.input.clear();
        self.sel = 0;
        self.top = 0;
        self.armed = None;
        self.notice = None;
        match mode {
            Mode::Windows => {
                self.wins = crate::switcher::list();
                // the one in front is first; the choice starts on the one before it
                self.wsel = if self.wins.len() > 1 { 1 } else { 0 };
            }
            Mode::Kill => self.wins = crate::switcher::list(),
            Mode::Run => self.run_hist = crate::launcher::bridge_run_history(),
            Mode::Search => self.ensure_index(bus),
            Mode::Clip | Mode::Ai => {}
        }
        self.refresh();
    }

    fn step(&mut self, d: i32) {
        let n = self.wins.len() as i32;
        if n > 0 {
            self.wsel = (self.wsel as i32 + d).rem_euclid(n) as usize;
        }
    }

    fn notify(&mut self, text: &str, bad: bool) {
        self.notice = Some((text.to_string(), Instant::now(), bad));
    }

    // -- what is listed -----------------------------------------------------------------------------------

    fn refresh(&mut self) {
        self.hits.clear();
        let q = self.input.text().trim().to_string();
        match self.mode {
            Mode::Search => self.list_search(&q),
            Mode::Clip => self.list_clip(&q),
            Mode::Kill => self.list_kill(&q),
            Mode::Run => self.list_run(&q),
            Mode::Ai | Mode::Windows => {}
        }
        self.sel = self.sel.min(self.hits.len().saturating_sub(1));
        self.fix_top();
    }

    /// The list scrolls just enough to keep the choice in view.
    fn fix_top(&mut self) {
        let n = self.hits.len();
        let max_top = n.saturating_sub(VIS);
        if self.sel < self.top {
            self.top = self.sel;
        } else if self.sel >= self.top + VIS {
            self.top = self.sel + 1 - VIS;
        }
        self.top = self.top.min(max_top);
    }

    fn list_search(&mut self, q: &str) {
        if q.is_empty() {
            // nothing typed: the apps used most
            let mut order: Vec<usize> = (0..self.apps.len()).collect();
            order.sort_by_key(|&i| (std::cmp::Reverse(self.usage.get(&self.apps[i].1).copied().unwrap_or(0)), i));
            for i in order.into_iter().take(KEEP) {
                let (name, path) = &self.apps[i];
                self.hits.push(Hit::new(K::App, name.clone(), app_detail(path), path.clone()));
            }
            return;
        }
        if let Some((result, detail)) = crate::launcher::bridge_answer(q) {
            self.hits.push(Hit::new(K::Calc, result, detail, String::new()));
        } else if let Some(u) = crate::launcher::bridge_url(q) {
            self.hits.push(Hit::new(K::Url, u.clone(), "Open in the browser".into(), u));
        }
        // "restart", "shut down", "lock" ...
        for (i, name, twice) in crate::launcher::bridge_system(q) {
            let mut h = Hit::new(K::System, name, "System".into(), format!("sys:{i}"));
            h.idx = i;
            h.id = u64::from(twice);
            self.hits.push(h);
        }
        let ql = q.to_ascii_lowercase();
        let qb = &ql.as_bytes()[..ql.len().min(40)];
        let na = self.apps.len();
        let mut pos: Vec<usize> = Vec::new();
        let mut best: Vec<(i32, usize)> = Vec::with_capacity(KEEP + 1);
        for k in 0..na + self.files.len() {
            let (name, path, kind) = if k < na {
                (self.apps[k].0.as_str(), self.apps[k].1.as_str(), K::App)
            } else {
                let f = &self.files[k - na];
                (file_name(&f.0), f.0.as_str(), if f.1 { K::Folder } else { K::File })
            };
            let Some(s) = crate::launcher::bridge_match(name, qb, k < na || qb.len() >= 3, &mut pos) else { continue };
            let mut s = s - name.len().min(80) as i32;
            s += match kind {
                K::App => 800,
                K::Folder => 100,
                _ => 0,
            };
            if let Some(&n) = self.usage.get(path) {
                s += n.min(20) as i32 * 12;
            }
            if best.len() < KEEP || s > best[best.len() - 1].0 {
                let at = best.partition_point(|b| b.0 >= s);
                best.insert(at, (s, k));
                best.truncate(KEEP);
            }
        }
        for &(_, k) in &best {
            if k < na {
                let (name, path) = &self.apps[k];
                self.hits.push(Hit::new(K::App, name.clone(), app_detail(path), path.clone()));
            } else {
                let (path, dir) = &self.files[k - na];
                let kind = if *dir { K::Folder } else { K::File };
                self.hits.push(Hit::new(kind, file_name(path).to_string(), path.clone(), path.clone()));
            }
        }
        let url = format!("{}{}", self.search_url, crate::launcher::bridge_enc(q));
        self.hits.push(Hit::new(
            K::Web,
            format!("Search the web for \u{201C}{}\u{201D}", crate::launcher::bridge_mid(q, 40)),
            "Web search".into(),
            url,
        ));
    }

    fn list_clip(&mut self, q: &str) {
        for r in crate::cliphist::rows(q).into_iter().take(60) {
            let mut h = Hit::new(K::Clip, r.summary, age_text(r.age_secs), String::new());
            h.id = r.id;
            h.idx = r.code as usize;
            h.thumb = r.thumb;
            self.hits.push(h);
        }
    }

    fn list_kill(&mut self, q: &str) {
        let ql = q.to_lowercase();
        let mut seen: Vec<String> = Vec::new();
        for w in &self.wins {
            let key = w.exe.to_lowercase();
            if w.exe.is_empty() || seen.contains(&key) {
                continue;
            }
            let app = w.app();
            if !ql.is_empty() && !app.to_lowercase().contains(&ql) && !w.title.to_lowercase().contains(&ql) {
                continue;
            }
            seen.push(key);
            self.hits.push(Hit::new(K::Prog, app, w.title.clone(), w.exe.clone()));
        }
    }

    fn list_run(&mut self, q: &str) {
        let ql = q.to_lowercase();
        for c in self.run_hist.iter().filter(|c| ql.is_empty() || c.to_lowercase().contains(&ql)) {
            self.hits.push(Hit::new(K::Cmd, c.clone(), "Recent command".into(), c.clone()));
        }
    }

    // -- choosing -------------------------------------------------------------------------------------------

    fn move_sel(&mut self, d: i32) {
        let n = self.hits.len();
        if n == 0 {
            return;
        }
        self.sel = (self.sel as i32 + d).clamp(0, n as i32 - 1) as usize;
        self.armed = None;
        self.fix_top();
    }

    fn commit_window(&mut self, fx: &mut Effects) {
        let Some(h) = self.wins.get(self.wsel).map(|w| w.hwnd) else { return };
        self.restore_focus = false;
        sys::activate_window(h);
        fx.collapse = true;
    }

    /// Enter on the choice (`shift`: the other thing to do: copy instead of paste, force quit, show in folder).
    fn run(&mut self, shift: bool, fx: &mut Effects, bus: &Bus) {
        match self.mode {
            Mode::Windows => return self.commit_window(fx),
            Mode::Ai => return self.ask(fx, bus),
            Mode::Run => return self.run_command(fx),
            _ => {}
        }
        let Some(h) = self.hits.get(self.sel).cloned() else { return };
        let hwnd = self.hwnd.hwnd();
        match h.kind {
            K::App | K::File | K::Folder => {
                if shift && h.kind != K::App {
                    sys::shell_reveal(&h.path);
                } else {
                    sys::shell_open(&h.path);
                }
                *self.usage.entry(h.path.clone()).or_insert(0) += 1;
                self.restore_focus = false;
                fx.collapse = true;
            }
            K::Url | K::Web => {
                sys::shell_open(&h.path);
                self.restore_focus = false;
                fx.collapse = true;
            }
            K::Calc => {
                clip::set_text(hwnd, &h.name);
                fx.collapse = true;
            }
            K::System => {
                // switching the computer off asks twice
                if h.id == 1 && self.armed != Some(h.idx) {
                    self.armed = Some(h.idx);
                    return;
                }
                crate::launcher::bridge_run_system(h.idx);
                self.restore_focus = false;
                fx.collapse = true;
            }
            K::Clip => {
                if crate::cliphist::copy_back(hwnd, h.id) && !shift {
                    // the window that had the focus gets it back, then the paste follows
                    if self.prev_fg != 0 {
                        sys::activate_window(self.prev_fg);
                    }
                    self.restore_focus = false;
                    crate::launcher::bridge_paste();
                }
                fx.collapse = true;
            }
            K::Prog => {
                crate::launcher::bridge_quit(&h.path, shift);
                self.notify(if shift { "Ended" } else { "Asked to quit" }, false);
                // the list keeps its place; the program leaves it after a moment
                self.wins.retain(|w| !w.exe.eq_ignore_ascii_case(&h.path));
                self.refresh();
            }
            K::Cmd => {
                let c = h.path.clone();
                self.start_command(&c, fx);
            }
        }
    }

    fn run_command(&mut self, fx: &mut Effects) {
        let typed = self.input.text().trim().to_string();
        let cmd = if typed.is_empty() { self.hits.get(self.sel).map(|h| h.path.clone()).unwrap_or_default() } else { typed };
        if cmd.is_empty() {
            return;
        }
        self.start_command(&cmd, fx);
    }

    fn start_command(&mut self, cmd: &str, fx: &mut Effects) {
        if sys::shell_run(cmd) {
            self.run_hist.retain(|c| c != cmd);
            self.run_hist.insert(0, cmd.to_string());
            self.run_hist.truncate(RUN_KEEP);
            crate::launcher::bridge_save_run_history(&self.run_hist);
            self.restore_focus = false;
            fx.collapse = true;
        } else {
            self.notify("Windows can't find that", true);
        }
    }

    /// The question goes to the Gemini page, which answers it there.
    fn ask(&mut self, fx: &mut Effects, bus: &Bus) {
        let q = self.input.text().trim().to_string();
        if q.is_empty() {
            return;
        }
        bus.to_module("gemini", super::gemini::Ask(q));
        fx.open_page = Some("gemini");
        fx.keyboard_focus = Some(true);
        fx.redraw = true;
    }

    /// Text was typed.
    fn typed(&mut self, bus: &Bus) {
        if self.mode == Mode::Search {
            // ">" starts the Run box, "?" asks the AI
            match self.input.text().as_str() {
                ">" => return self.set_mode(Mode::Run, bus),
                "?" => return self.set_mode(Mode::Ai, bus),
                _ => {}
            }
        }
        self.sel = 0;
        self.top = 0;
        self.armed = None;
        self.refresh();
    }

    // -- drawing ----------------------------------------------------------------------------------------------

    fn draw_empty(&mut self, ui: &mut Ui, list: Rect, since: f32) {
        let (icon, msg) = match self.mode {
            Mode::Search if self.input.is_empty() && self.apps.is_empty() => (Icon::Grid, "Looking for your apps\u{2026}"),
            Mode::Search => (Icon::Grid, "Nothing found"),
            Mode::Clip => (Icon::Clipboard, "Nothing copied yet"),
            Mode::Kill => (Icon::Cpu, "No apps to quit"),
            Mode::Run => (Icon::Keyboard, "Type a command and press Enter"),
            Mode::Ai if self.input.is_empty() => (Icon::Sparkle, "Ask a question and press Enter"),
            Mode::Ai => (Icon::Sparkle, "Press Enter to ask Gemini"),
            Mode::Windows => (Icon::Image, "No open windows"),
        };
        let a0 = ui.p.alpha;
        ui.p.alpha = a0 * ease_out((since / 0.28).clamp(0.0, 1.0));
        crate::alcove::empty(ui, list, icon, msg);
        ui.p.alpha = a0;
    }

    fn draw_list(&mut self, ui: &mut Ui, list: Rect, since: f32) -> bool {
        let n = self.hits.len();
        let a0 = ui.p.alpha;
        let mut busy = false;
        // the wheel moves the list a row at a time
        let w = ui.wheel(list);
        if w > 0.0 {
            self.top = self.top.saturating_sub(1);
        } else if w < 0.0 {
            self.top = (self.top + 1).min(n.saturating_sub(VIS));
        }
        let off = ui.glide(gkey("alcl-off", 0), self.top as f32);
        let sel_y = ui.glide(gkey("alcl-sel", 0), self.sel as f32);
        let first = off.floor().max(0.0) as usize;
        let last = ((off + VIS as f32).ceil() as usize + 1).min(n);
        ui.p.push_clip(list);
        // the choice: a soft light that glides from row to row
        let pill = Rect::new(list.x, list.y + (sel_y - off) * ROW, list.w, ROW - 4.0);
        ui.p.fill_rounded(pill, 14.0, Color::white(0.13));
        let mut clicked = None;
        for i in first..last {
            let rk = ease_out(((since - (i.saturating_sub(self.top)).min(8) as f32 * 0.025) / 0.24).clamp(0.0, 1.0));
            if rk < 1.0 {
                busy = true;
            }
            let row = Rect::new(list.x, list.y + (i as f32 - off) * ROW + (1.0 - rk) * 10.0, list.w, ROW - 4.0);
            ui.p.alpha = a0 * rk;
            let h = &self.hits[i];
            let hover = ui.hovered(row);
            if hover {
                ui.hot = true;
                if i != self.sel {
                    ui.p.fill_rounded(row, 14.0, Color::white(0.06));
                }
            }
            // the picture
            let ib = Rect::new(row.x + 12.0, row.cy() - 15.0, 30.0, 30.0);
            let shell = matches!(h.kind, K::App | K::File | K::Folder | K::Prog);
            let img = if shell { self.icons.get(&h.path, &h.name) } else { h.thumb.clone() };
            match img {
                Some(img) => ui.p.image(&img, ib, 7.0, 1.0),
                None => {
                    ui.p.fill_rounded(ib, 9.0, Color::white(0.10));
                    ui.p.icon(kind_icon(h), ib.inset(6.5), Color::white(0.9));
                }
            }
            // the words
            let tx = ib.right() + 12.0;
            let tw = (row.right() - tx - 104.0).max(40.0);
            let big = h.kind == K::Calc;
            ui.p.push_clip(Rect::new(tx, row.y, tw, row.h));
            let name_st = if big { TextStyle::new(18.0, palette::TEXT).semibold() } else { TextStyle::new(14.0, palette::TEXT).medium() };
            ui.p.text(&h.name, Rect::new(tx, row.y + 3.0, tw + 100.0, 22.0), &name_st);
            let (detail, dc) = if h.kind == K::System && self.armed == Some(h.idx) {
                ("Press Enter again to confirm".to_string(), palette::ORANGE)
            } else {
                (h.detail.clone(), palette::TEXT_FAINT)
            };
            ui.p.text(&detail, Rect::new(tx, row.y + 23.0, tw + 100.0, 16.0), &TextStyle::new(11.5, dc));
            ui.p.pop_clip();
            // what Enter does
            let side = if i == self.sel { action_word(h) } else { kind_word(h) };
            let a = if i == self.sel { 0.75 } else { 0.38 };
            ui.p.text(
                side,
                Rect::new(row.right() - 100.0, row.y, 88.0, row.h),
                &TextStyle::new(11.5, Color::white(a)).medium().right(),
            );
            if ui.clicked(row) {
                clicked = Some(i);
            }
        }
        ui.p.alpha = a0;
        ui.p.pop_clip();
        // a thin marker for a list that scrolls
        if n > VIS {
            let track = Rect::new(list.right() - 3.0, list.y + 2.0, 3.0, list.h - 4.0);
            let frac = VIS as f32 / n as f32;
            let th = (track.h * frac).max(14.0);
            let ty = track.y + (track.h - th) * (off / (n - VIS) as f32).clamp(0.0, 1.0);
            ui.p.fill_rounded(Rect::new(track.x, ty, track.w, th), 1.5, Color::white(0.22));
        }
        if let Some(i) = clicked {
            self.sel = i;
            self.armed = None;
            self.run(false, ui.fx, ui.bus);
        }
        busy
    }

    fn draw_windows(&mut self, ui: &mut Ui, list: Rect, since: f32) -> bool {
        let n = self.wins.len();
        let a0 = ui.p.alpha;
        let mut busy = false;
        let pitch = CARD_W + CARD_GAP;
        let total = n as f32 * pitch - CARD_GAP;
        let target = if total <= list.w {
            -(list.w - total) * 0.5
        } else {
            (self.wsel as f32 * pitch - (list.w - CARD_W) * 0.5).clamp(0.0, total - list.w)
        };
        let off = ui.glide(gkey("alcl-woff", 0), target);
        let sel_x = ui.glide(gkey("alcl-wsel", 0), self.wsel as f32);
        ui.p.push_clip(list);
        let ring = Rect::new(list.x - off + sel_x * pitch, list.y, CARD_W, CARD_H);
        ui.p.fill_rounded(ring, 18.0, Color::white(0.15));
        ui.p.stroke_rounded(ring.inset(0.5), 17.5, 1.0, Color::white(0.38));
        let mut clicked = None;
        for i in 0..n {
            let x = list.x - off + i as f32 * pitch;
            if x + CARD_W < list.x - 4.0 || x > list.right() + 4.0 {
                continue;
            }
            let rk = ease_out(((since - i.min(8) as f32 * 0.03) / 0.26).clamp(0.0, 1.0));
            if rk < 1.0 {
                busy = true;
            }
            let card = Rect::new(x, list.y + (1.0 - rk) * 12.0, CARD_W, CARD_H);
            ui.p.alpha = a0 * rk;
            let w = &self.wins[i];
            if ui.hovered(card) {
                ui.hot = true;
                if i != self.wsel {
                    ui.p.fill_rounded(card, 18.0, Color::white(0.06));
                }
            }
            let ib = Rect::new(card.x + 14.0, card.y + 14.0, 36.0, 36.0);
            let app = w.app();
            match self.icons.get(&w.exe, &app) {
                Some(img) => ui.p.image(&img, ib, 8.0, 1.0),
                None => {
                    ui.p.fill_rounded(ib, 10.0, Color::white(0.10));
                    ui.p.icon(Icon::Image, ib.inset(8.0), Color::white(0.9));
                }
            }
            if w.minimized {
                ui.p.text(
                    "Minimized",
                    Rect::new(card.x + 56.0, card.y + 14.0, CARD_W - 68.0, 14.0),
                    &TextStyle::new(10.5, palette::TEXT_FAINT).right(),
                );
            }
            let title = if w.title.trim().is_empty() { app.clone() } else { w.title.clone() };
            ui.p.push_clip(Rect::new(card.x + 12.0, card.y, CARD_W - 24.0, card.h));
            ui.p.text(&title, Rect::new(card.x + 14.0, card.y + 58.0, CARD_W + 80.0, 20.0), &TextStyle::new(12.5, palette::TEXT).medium());
            ui.p.text(&app, Rect::new(card.x + 14.0, card.y + 78.0, CARD_W + 80.0, 16.0), &TextStyle::new(11.0, palette::TEXT_FAINT));
            ui.p.pop_clip();
            if ui.clicked(card) {
                clicked = Some(i);
            }
        }
        ui.p.alpha = a0;
        ui.p.pop_clip();
        if let Some(i) = clicked {
            self.wsel = i;
            self.commit_window(ui.fx);
        }
        busy
    }
}

impl Module for AlcoveLauncher {
    fn id(&self) -> ModuleId {
        ID
    }

    fn title(&self) -> &str {
        "Alcove launcher"
    }

    fn icon(&self) -> Icon {
        Icon::Grid
    }

    fn start(&mut self, cx: &mut Cx) {
        self.hwnd = cx.hwnd;
        self.search_url = cx.cfg.launcher.search_url.clone();
        self.files_on = cx.cfg.launcher.files;
        self.folders = cx.cfg.launcher.folders.clone();
    }

    fn stop(&mut self) {
        self.free();
        self.open = false;
    }

    fn on_message(&mut self, msg: Box<dyn Any + Send>, cx: &mut Cx) {
        let Ok(m) = msg.downcast::<Msg>() else { return };
        match *m {
            Msg::Apps(list) => self.apps = list,
            Msg::Files(list) => self.files = list,
        }
        if self.open && self.mode == Mode::Search {
            self.refresh();
            cx.fx.redraw = true;
        }
    }

    fn on_system(&mut self, ev: &SystemEvent, cx: &mut Cx) {
        match ev {
            SystemEvent::Command { verb, args } if verb == "alcove-launcher" => {
                let what = args.first().map(String::as_str).unwrap_or("search");
                let showing = args.get(1).is_some_and(|s| s == "open");
                self.launch(what, showing, cx);
            }
            SystemEvent::Expanded(false) => self.closed(),
            SystemEvent::ConfigReloaded => {
                self.search_url = cx.cfg.launcher.search_url.clone();
                self.files_on = cx.cfg.launcher.files;
                self.folders = cx.cfg.launcher.folders.clone();
            }
            _ => {}
        }
    }

    fn next_tick(&self) -> Option<Instant> {
        if self.open {
            // the switcher opened with Alt held: watch for Alt being let go
            return (self.alt_open && self.mode == Mode::Windows).then(|| Instant::now() + Duration::from_millis(40));
        }
        self.closed_at.filter(|_| self.holds_memory()).map(|t| t + FREE_AFTER)
    }

    fn on_tick(&mut self, cx: &mut Cx) {
        if self.open {
            if self.alt_open && self.mode == Mode::Windows && !alt_down() {
                self.alt_open = false;
                self.commit_window(cx.fx);
                cx.fx.redraw = true;
            }
        } else if self.closed_at.is_some_and(|t| t.elapsed() >= FREE_AFTER) {
            self.free();
        }
    }

    fn has_page(&self) -> bool {
        true
    }

    fn wants_keyboard(&self) -> bool {
        true
    }

    fn page_height(&self, _width: f32) -> f32 {
        let top = HEAD + 2.0 + CHIPS + GAP;
        let body = match self.mode {
            Mode::Windows if !self.wins.is_empty() => CARD_H + 6.0,
            Mode::Windows | Mode::Ai => 96.0,
            _ if self.hits.is_empty() => 96.0,
            _ => self.hits.len().min(VIS) as f32 * ROW + 4.0,
        };
        top + body
    }

    fn on_key(&mut self, key: Key, cx: &mut Cx) {
        cx.fx.redraw = true;
        if self.mode == Mode::Windows {
            match key {
                Key::Tab | Key::Right | Key::Down => self.step(1),
                Key::Left | Key::Up => self.step(-1),
                Key::Home => self.wsel = 0,
                Key::End => self.wsel = self.wins.len().saturating_sub(1),
                Key::Enter { .. } => self.commit_window(cx.fx),
                _ => {}
            }
            return;
        }
        match key {
            Key::Up => self.move_sel(-1),
            Key::Down => self.move_sel(1),
            Key::Tab => {
                let next = self.mode.next();
                self.set_mode(next, cx.bus);
            }
            Key::Enter { shift } => self.run(shift, cx.fx, cx.bus),
            // nothing left to delete in another mode: back to searching
            Key::Backspace if self.input.is_empty() && self.mode != Mode::Search => self.set_mode(Mode::Search, cx.bus),
            k => {
                let before = self.input.text();
                self.input.key(k, cx.hwnd);
                if self.input.text() != before {
                    self.typed(cx.bus);
                }
            }
        }
    }

    fn draw_page(&mut self, ui: &mut Ui, r: Rect) {
        self.icons.budget = 3;
        self.icons.starved = false;
        let a0 = ui.p.alpha;
        let since = ui.now.saturating_duration_since(self.t_list).as_secs_f32();
        let mode_t = ui.now.saturating_duration_since(self.t_mode).as_secs_f32();
        let mut busy = since < 0.6 || mode_t < 0.35;

        // the icon of the mode pops in, then the field
        let head = Rect::new(r.x, r.y, r.w, HEAD);
        let grow = smoothstep(0.0, 0.4, since);
        let ease = 1.0 - (1.0 - grow) * (1.0 - grow) * (1.0 - grow);
        let pw = r.w * (0.7 + 0.3 * ease);
        let pill = Rect::new(r.cx() - pw * 0.5, head.y + 3.0, pw, HEAD - 6.0);
        ui.p.fill_rounded(pill, pill.h * 0.5, crate::util::Color::black(1.0));
        ui.p.stroke_rounded(pill.inset(0.5), pill.h * 0.5 - 0.5, 1.0, crate::util::Color::white(0.10));
        let pop = smoothstep(0.0, 0.3, mode_t);
        ui.p.alpha = a0 * pop;
        let ico = Rect::new(pill.x + 14.0, pill.cy() - 9.0, 18.0, 18.0);
        ui.p.icon(self.mode.icon(), ico.center_square(18.0 * (0.7 + 0.3 * pop)), palette::TEXT);
        ui.p.alpha = a0;
        let field = Rect::new(pill.x + 42.0, head.y, (pill.w - 56.0).max(40.0), head.h);
        let hint = if self.mode == Mode::Search { "Search app..." } else { self.mode.hint() };
        self.input.draw_bare(ui, field, hint, true, 18.0);

        // the modes
        let cy = head.bottom() + 2.0;
        let x = r.x;
        // a short message beside the chips (copied, can't find ...)
        let mut expired = false;
        if let Some((msg, at, bad)) = &self.notice {
            if at.elapsed() < Duration::from_millis(2500) {
                let c = if *bad { palette::RED } else { palette::TEXT_DIM };
                if x + 80.0 < r.right() {
                    ui.p.text(msg, Rect::new(x + 6.0, cy, r.right() - x - 6.0, CHIPS), &TextStyle::new(12.0, c).medium().right());
                }
                ui.animate_slow();
            } else {
                expired = true;
            }
        }
        if expired {
            self.notice = None;
        }

        // the list, the cards or an empty message
        let top = cy + CHIPS + GAP;
        let list = Rect::new(r.x, top, r.w, (r.bottom() - top).max(0.0));
        let since = ui.now.saturating_duration_since(self.t_list).as_secs_f32();
        let shown = match self.mode {
            Mode::Windows if !self.wins.is_empty() => self.draw_windows(ui, list, since),
            Mode::Ai | Mode::Windows => {
                self.draw_empty(ui, list, since);
                false
            }
            _ if self.hits.is_empty() => {
                self.draw_empty(ui, list, since);
                false
            }
            _ => self.draw_list(ui, list, since),
        };
        busy |= shown || self.icons.starved;
        if busy {
            ui.animate();
        }
    }
}

fn alt_down() -> bool {
    let state = unsafe { GetAsyncKeyState(VK_MENU.0 as i32) };
    state as u16 & 0x8000 != 0
}

fn file_name(p: &str) -> &str {
    p.rsplit(['\\', '/']).next().unwrap_or(p)
}

fn app_detail(path: &str) -> String {
    if path.starts_with("shell:") {
        "Store app".to_string()
    } else {
        "Application".to_string()
    }
}

fn age_text(secs: u64) -> String {
    match secs {
        0..=59 => "Just now".to_string(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86_399 => format!("{} h ago", secs / 3600),
        _ => format!("{} d ago", secs / 86_400),
    }
}

fn kind_icon(h: &Hit) -> Icon {
    match h.kind {
        K::App => Icon::Package,
        K::Folder => Icon::Folder,
        K::File => Icon::File,
        K::Calc => Icon::Bolt,
        K::Url | K::Web => Icon::Globe,
        K::System => Icon::Settings,
        K::Clip => match h.idx {
            1 => Icon::Link,
            2 => Icon::Image,
            3 => Icon::Folder,
            _ => Icon::Clipboard,
        },
        K::Prog => Icon::Cpu,
        K::Cmd => Icon::Keyboard,
    }
}

/// The small word at the right of a row that is not chosen.
fn kind_word(h: &Hit) -> &'static str {
    match h.kind {
        K::App => "App",
        K::Folder => "Folder",
        K::File => "File",
        K::Calc => "Answer",
        K::Url => "Link",
        K::Web => "Web",
        K::System => "System",
        K::Clip => "Clipboard",
        K::Prog => "Running",
        K::Cmd => "Recent",
    }
}

/// What Enter does with the chosen row.
fn action_word(h: &Hit) -> &'static str {
    match h.kind {
        K::App | K::File | K::Folder | K::Url => "Open  \u{21B5}",
        K::Web => "Search  \u{21B5}",
        K::Calc => "Copy  \u{21B5}",
        K::System => "Run  \u{21B5}",
        K::Clip => "Paste  \u{21B5}",
        K::Prog => "Quit  \u{21B5}",
        K::Cmd => "Run  \u{21B5}",
    }
}
