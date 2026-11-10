//! Settings tab: the options people change most, without opening config.toml.
//!
//! Every control writes through `Effects::save_config`, which edits one value
//! in config.toml (keeping comments) and lets the normal hot reload apply it.
//! The Gemini key goes to `%APPDATA%\DynamicNotch\.env`, never to the config.

use std::any::Any;
use std::time::{Duration, Instant};

use super::gemini;
use super::widgets::{chip, segmented, slider, switch, toml_str, TextField};
use super::{Cx, Key, Module, ModuleId, SystemEvent};
use crate::config::{self, Config};
use crate::gfx::{Icon, TextStyle};
use crate::sys::{dotenv, shell_open};
use crate::ui::{ButtonStyle, Ui};
use crate::updater;
use crate::util::{app_dir, palette, Color, Rect, SendHwnd};

const ID: ModuleId = "settings";
const TABS: [&str; 12] = [
    "General", "Look", "Home", "Modules", "Weather", "Gemini", "Notch", "Tabs", "Dock", "Glass", "Combos", "Radial",
];
const ROW_H: f32 = 36.0;
const ROW_GAP: f32 = 4.0;
const TAB_H: f32 = 32.0;
const FOOT_H: f32 = 22.0;
const MAX_HOME: usize = 4;
const MAX_NOTCH: usize = 3;

/// Widgets that can sit on the closed notch: (config id, label).
const NOTCH_CHOICES: [(&str, &str); 7] = [
    ("clock", "Clock"),
    ("date", "Date"),
    ("battery", "Battery"),
    ("weather", "Weather"),
    ("wifi", "Wi-Fi"),
    ("bluetooth", "Bluetooth"),
    ("notes", "Notes"),
];

/// Widgets that can be placed on the home page: (config id, label).
const HOME_CHOICES: [(&str, &str); 9] = [
    ("weather", "Weather"),
    ("stats", "System stats"),
    ("timer", "Timer"),
    ("control", "Wi-Fi & Bluetooth"),
    ("notes", "Notes"),
    ("clipboard", "Clipboard"),
    ("downloads", "Downloads"),
    ("shelf", "Shelf"),
    ("ports", "Dev servers"),
];

/// Modules that can be switched on and off: (config id, label).
const MODULE_CHOICES: [(&str, &str); 21] = [
    ("media", "Media"),
    ("timer", "Timer"),
    ("downloads", "Downloads"),
    ("clipboard", "Clipboard"),
    ("shelf", "Shelf"),
    ("voice", "Dictation"),
    ("ai", "Ask AI"),
    ("gemini", "Gemini"),
    ("weather", "Weather"),
    ("control", "Wi-Fi & Bluetooth"),
    ("notifications", "Notifications"),
    ("mirror", "Mirror (camera tab)"),
    ("notes", "Notes"),
    ("greeting", "Unlock greeting"),
    ("stats", "System stats"),
    ("ports", "Dev servers"),
    ("privacy", "Mic & camera"),
    ("battery", "Battery"),
    ("locks", "Lock keys"),
    ("osd", "Volume & brightness"),
    ("devices", "USB drives"),
];

/// Tabs that can be reordered: (config id, label), in the default order.
const TAB_CHOICES: [(&str, &str); 13] = [
    ("timer", "Timer"),
    ("voice", "Dictation"),
    ("clipboard", "Clipboard"),
    ("downloads", "Downloads"),
    ("shelf", "Shelf"),
    ("ports", "Dev Servers"),
    ("ai", "Ask AI"),
    ("gemini", "Gemini"),
    ("weather", "Weather"),
    ("control", "Wi-Fi & BT"),
    ("notes", "Notes"),
    ("notifications", "Notifications"),
    ("mirror", "Mirror"),
];

const SWATCHES: [u32; 6] = [0x0A84FF, 0xBF5AF2, 0xFF375F, 0xFF9F0A, 0x34C759, 0x64D2FF];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    None,
    City,
    Name,
    Key,
    Model,
    ComboTarget,
    ComboFind,
}

/// Where the "Check for updates" button stands.
enum Update {
    Idle,
    Checking,
    UpToDate,
    Available { tag: String, installer: Option<String>, page: String, notes: String },
    Installing,
    Failed(String),
}

pub struct Settings {
    tab: usize,
    update: Update,
    cfg: Config,
    city: TextField,
    name: TextField,
    key: TextField,
    model: TextField,
    /// The keys recorded for a new combo ("Win+B"), or nothing yet.
    combo_keys: String,
    combo_target: TextField,
    /// Searching the list of apps.
    combo_find: TextField,
    /// The name and path of an app picked from the list.
    combo_label: String,
    combo_path: String,
    picker: bool,
    /// The slice of the radial menu the apps menu is choosing for.
    radial_slot: Option<usize>,
    pick_scroll: f32,
    apps: std::sync::Arc<std::sync::Mutex<Option<Vec<(String, String)>>>>,
    app_list: Vec<(String, String)>,
    apps_started: bool,
    apps_loaded: bool,
    /// Waiting for the user to press the keys.
    recording: bool,
    focus: Focus,
    /// How far the current tab is scrolled (a whole number of rows) when it is taller than the page.
    scroll: f32,
    has_key: bool,
    status: Option<(String, Instant, bool)>,
    hwnd: SendHwnd,
}

impl Settings {
    pub fn new() -> Self {
        Self {
            tab: 0,
            update: Update::Idle,
            cfg: Config::default(),
            city: TextField::new(80),
            name: TextField::new(24),
            key: TextField::new(200).masked(),
            model: TextField::new(80),
            combo_keys: String::new(),
            combo_target: TextField::new(260),
            combo_find: TextField::new(60),
            combo_label: String::new(),
            combo_path: String::new(),
            picker: false,
            radial_slot: None,
            pick_scroll: 0.0,
            apps: std::sync::Arc::new(std::sync::Mutex::new(None)),
            app_list: Vec::new(),
            apps_started: false,
            apps_loaded: false,
            recording: false,
            focus: Focus::None,
            scroll: 0.0,
            has_key: false,
            status: None,
            hwnd: SendHwnd(0),
        }
    }

    fn say(&mut self, text: &str, good: bool) {
        self.status = Some((text.to_string(), Instant::now(), good));
    }

    fn sync_fields(&mut self) {
        if self.focus != Focus::City {
            self.city.set(&self.cfg.weather.city);
        }
        if self.focus != Focus::Name {
            self.name.set(&self.cfg.greeting.name);
        }
        if self.focus != Focus::Model {
            self.model.set(&self.cfg.gemini.model);
        }
    }

    fn save_key(&mut self, bus: &crate::bus::Bus) {
        let key: String = self.key.text().chars().filter(|c| !c.is_whitespace()).collect();
        if key.len() < 20 {
            self.say("That doesn't look like a Gemini API key", false);
        } else if dotenv::set_saved(gemini::KEY_VAR, &key) {
            self.key.clear();
            self.has_key = true;
            bus.to_module("gemini", gemini::KeyChanged);
            self.say("Key saved", true);
        } else {
            self.say("Couldn't save the key", false);
        }
    }
}

fn put(ui: &mut Ui, section: &str, key: &str, value: String) {
    ui.fx.save_config.push((section.to_string(), key.to_string(), value));
}

fn list_value(ids: &[String]) -> String {
    format!("[{}]", ids.iter().map(|s| toml_str(s)).collect::<Vec<_>>().join(", "))
}

/// "Order" grid for a list of items: each one is a capsule with ‹ › arrows to
/// move it earlier or later. `row` is the first row (its height is the row
/// height). Returns the new order when something moved.
fn order_row(ui: &mut Ui, row: Rect, ids: &[String], names: &[(&str, &str)], cols: usize) -> Option<Vec<String>> {
    if ids.len() < 2 {
        return None;
    }
    let gap = 8.0;
    let cols = cols.max(1);
    let w = (row.w - gap * (cols as f32 - 1.0)) / cols as f32;
    let mut result = None;
    for (i, id) in ids.iter().enumerate() {
        let r = Rect::new(row.x + (i % cols) as f32 * (w + gap), row.y + (i / cols) as f32 * (row.h + gap), w, row.h);
        ui.p.fill_rounded(r, r.h * 0.5, Color::white(0.07));
        let label = names.iter().find(|(k, _)| *k == id.as_str()).map(|(_, n)| *n).unwrap_or(id.as_str());
        ui.p.text(
            label,
            Rect::new(r.x + 28.0, r.y, r.w - 56.0, r.h),
            &TextStyle::new(12.0, palette::TEXT).medium().center(),
        );
        for (dir, can) in [(-1i32, i > 0), (1i32, i + 1 < ids.len())] {
            let a = if dir < 0 {
                Rect::new(r.x + 3.0, r.y + 3.0, 24.0, r.h - 6.0)
            } else {
                Rect::new(r.right() - 27.0, r.y + 3.0, 24.0, r.h - 6.0)
            };
            let hover = can && ui.hovered(a);
            if hover {
                ui.hot = true;
                ui.p.fill_rounded(a, a.h * 0.5, Color::white(0.14));
            }
            let col = if can { palette::TEXT } else { palette::TEXT_FAINT.with_a(0.4) };
            ui.p.text(if dir < 0 { "‹" } else { "›" }, a, &TextStyle::new(17.0, col).medium().center());
            if can && ui.clicked(a) {
                let mut next = ids.to_vec();
                let j = (i as i32 + dir) as usize;
                next.swap(i, j);
                result = Some(next);
            }
        }
    }
    result
}

/// The tab order to show: the saved order (known ids only), then anything missing.
fn tab_order(saved: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in saved {
        if TAB_CHOICES.iter().any(|(k, _)| *k == id.as_str()) && !out.contains(id) {
            out.push(id.clone());
        }
    }
    for (k, _) in TAB_CHOICES {
        if !out.iter().any(|o| o == k) {
            out.push(k.to_string());
        }
    }
    out
}

/// How many rows each tab holds (the page grows to fit, then scrolls once the window is full).
fn tab_rows(tab: usize) -> f32 {
    match tab {
        0 => 14.0,
        1 => 22.0,
        2 => 5.6,
        3 => 5.5,
        4 => 4.0,
        5 => 4.0,
        6 => 5.6,
        7 => 5.9,
        8 => 24.0,
        9 => 33.0,
        10 => 14.0,
        11 => 22.0,
        _ => 25.0,
    }
}

fn row_rect(body: Rect, i: usize) -> Rect {
    Rect::new(body.x, body.y + i as f32 * (ROW_H + ROW_GAP), body.w, ROW_H)
}

fn label(ui: &mut Ui, row: Rect, text: &str) {
    ui.row_bg(row, false, None);
    ui.p.text(text, Rect::new(row.x + 12.0, row.y, row.w * 0.5, row.h), &TextStyle::new(13.0, palette::TEXT));
}

fn label_row(ui: &mut Ui, row: Rect, text: &str) {
    label(ui, row, text);
}

fn toggle_row(ui: &mut Ui, row: Rect, text: &str, on: bool, section: &str, key: &str) {
    label(ui, row, text);
    let sw = Rect::new(row.right() - 56.0, row.y, 48.0, row.h);
    let accent = ui.accent;
    if switch(ui, sw, on, accent) {
        put(ui, section, key, (!on).to_string());
    }
}

#[allow(clippy::too_many_arguments)]
fn slider_row(
    ui: &mut Ui,
    row: Rect,
    text: &str,
    val: f32,
    (lo, hi): (f32, f32),
    shown: String,
    section: &str,
    key: &str,
) {
    label(ui, row, text);
    ui.p.text(
        &shown,
        Rect::new(row.right() - 54.0, row.y, 46.0, row.h),
        &TextStyle::new(12.0, palette::TEXT_DIM).right(),
    );
    let sl = Rect::new(row.right() - 54.0 - 176.0, row.cy() - 12.0, 164.0, 24.0);
    let frac = ((val - lo) / (hi - lo)).clamp(0.0, 1.0);
    let accent = ui.accent;
    if let Some(f) = slider(ui, sl, frac, accent) {
        put(ui, section, key, format!("{:.2}", lo + f * (hi - lo)));
    }
}

impl Module for Settings {
    fn id(&self) -> ModuleId {
        ID
    }
    fn title(&self) -> &str {
        "Settings"
    }
    fn icon(&self) -> Icon {
        Icon::Settings
    }

    fn start(&mut self, cx: &mut Cx) {
        self.hwnd = cx.hwnd;
        self.cfg = cx.cfg.clone();
        self.has_key = dotenv::get_saved_first(gemini::KEY_VAR).is_some();
        self.sync_fields();
    }

    fn on_system(&mut self, ev: &SystemEvent, cx: &mut Cx) {
        match ev {
            SystemEvent::ConfigReloaded => {
                self.cfg = cx.cfg.clone();
                self.sync_fields();
                cx.fx.redraw = true;
            }
            SystemEvent::Expanded(false) => {
                self.focus = Focus::None;
                if self.recording {
                    self.recording = false;
                    crate::lowkeys::cancel_capture();
                }
            }
            // the keys were recorded: draw again, the page picks them up
            SystemEvent::Hotkey(id) if *id == crate::lowkeys::CAPTURE_ID => cx.fx.redraw = true,
            _ => {}
        }
    }

    fn on_message(&mut self, msg: Box<dyn Any + Send>, cx: &mut Cx) {
        let Ok(m) = msg.downcast::<updater::Msg>() else { return };
        self.update = match *m {
            updater::Msg::UpToDate => Update::UpToDate,
            updater::Msg::Available { tag, installer, page, notes } => Update::Available { tag, installer, page, notes },
            updater::Msg::Installing => Update::Installing,
            updater::Msg::Failed(why) => Update::Failed(why),
        };
        cx.fx.redraw = true;
    }

    fn has_page(&self) -> bool {
        true
    }

    fn wants_keyboard(&self) -> bool {
        self.focus != Focus::None
    }

    fn page_height(&self, _w: f32) -> f32 {
        TAB_H + 10.0 + tab_rows(self.tab) * (ROW_H + ROW_GAP) + FOOT_H
    }

    fn on_key(&mut self, key: Key, cx: &mut Cx) {
        let enter = match self.focus {
            Focus::City => self.city.key(key, self.hwnd),
            Focus::Name => self.name.key(key, self.hwnd),
            Focus::Key => self.key.key(key, self.hwnd),
            Focus::Model => self.model.key(key, self.hwnd),
            Focus::ComboFind => self.combo_find.key(key, self.hwnd),
            Focus::ComboTarget => self.combo_target.key(key, self.hwnd),
            Focus::None => false,
        };
        if enter && self.focus == Focus::ComboTarget {
            if let Some(list) = self.add_combo() {
                cx.fx.save_config.push(("combos".into(), "items".into(), list_value(&list)));
            }
            cx.fx.keyboard_focus = Some(false);
            self.focus = Focus::None;
            cx.fx.layout_changed = true;
            return;
        }
        if enter {
            match self.focus {
                Focus::City => {
                    let v = self.city.text().trim().to_string();
                    cx.fx.save_config.push(("weather".into(), "city".into(), toml_str(&v)));
                    self.say("City saved", true);
                }
                Focus::Name => {
                    let v = self.name.text().trim().to_string();
                    cx.fx.save_config.push(("greeting".into(), "name".into(), toml_str(&v)));
                    self.say("Name saved", true);
                }
                Focus::Key => self.save_key(cx.bus),
                Focus::Model => {
                    let v = self.model.text().trim().to_string();
                    if !v.is_empty() {
                        cx.fx.save_config.push(("gemini".into(), "model".into(), toml_str(&v)));
                        self.say("Model saved", true);
                    }
                }
                Focus::None | Focus::ComboTarget | Focus::ComboFind => {}
            }
            cx.fx.keyboard_focus = Some(false);
            self.focus = Focus::None;
        }
        cx.fx.layout_changed = true;
    }

    fn draw_page(&mut self, ui: &mut Ui, r: Rect) {
        let _accent = ui.accent;

        // --- sections ---------------------------------------------------------
        let gap = 6.0;
        let tw = (r.w - gap * (TABS.len() as f32 - 1.0)) / TABS.len() as f32;
        for (i, name) in TABS.iter().enumerate() {
            let b = Rect::new(r.x + i as f32 * (tw + gap), r.y, tw, TAB_H);
            let on = i == self.tab;
            let style = if on {
                ButtonStyle::inverted()
            } else {
                ButtonStyle::filled(Color::white(0.07)).fg(palette::TEXT_DIM)
            };
            let hit = if crate::ui::look_alcove() {
                crate::alcove::chip(ui, b, name, None, on)
            } else {
                ui.pill_button(b, name, None, style)
            };
            if hit && !on {
                self.tab = i;
                self.scroll = 0.0;
                self.picker = false;
                self.radial_slot = None;
                if self.recording {
                    self.recording = false;
                    crate::lowkeys::cancel_capture();
                }
                self.focus = Focus::None;
                ui.fx.keyboard_focus = Some(false);
                ui.fx.layout_changed = true;
            }
        }
        let body = Rect::new(r.x, r.y + TAB_H + 10.0, r.w, r.h - TAB_H - 10.0 - FOOT_H);

        // a tab taller than the page scrolls with the wheel, a row at a time
        let content_h = tab_rows(self.tab) * (ROW_H + ROW_GAP);
        let long = content_h > body.h + 0.5;
        let (page, was_live) = (body, ui.interactive);
        let body = if long {
            let shown =
                crate::alcove::scroll_rows(ui, page, &mut self.scroll, content_h, ROW_H + ROW_GAP, "settings-scroll");
            ui.p.push_clip(page);
            // rows scrolled out of view must not answer the pointer
            ui.interactive = was_live && ui.input.mouse.is_some_and(|(x, y)| page.contains(x, y));
            Rect::new(page.x, page.y - shown, page.w, content_h)
        } else {
            body
        };
        match self.tab {
            0 => self.general(ui, body),
            1 => self.look(ui, body),
            2 => self.home(ui, body),
            3 => self.modules(ui, body),
            4 => self.weather(ui, body),
            5 => self.gemini(ui, body),
            6 => self.notch(ui, body),
            7 => self.tabs(ui, body),
            8 => self.dock(ui, body),
            9 => self.glass(ui, body),
            10 => self.combos(ui, body),
            _ => self.radial(ui, body),
        }
        if long {
            ui.p.pop_clip();
            ui.interactive = was_live;
        }

        // --- status line ------------------------------------------------------
        if let Some((text, at, good)) = self.status.clone() {
            if at.elapsed() < Duration::from_secs(4) {
                let col = if good { palette::GREEN } else { palette::ORANGE };
                ui.p.text(&text, Rect::new(r.x, r.bottom() - FOOT_H, r.w, FOOT_H), &TextStyle::new(11.5, col).right());
            } else {
                self.status = None;
            }
        } else {
            ui.p.text(
                "Build v3.0.1 · Changes apply right away",
                Rect::new(r.x, r.bottom() - FOOT_H, r.w, FOOT_H),
                &TextStyle::new(11.0, palette::TEXT_FAINT).right(),
            );
        }
    }
}

impl Settings {
    fn general(&mut self, ui: &mut Ui, body: Rect) {
        let g = self.cfg.general.clone();
        let accent = ui.accent;
        toggle_row(
            ui,
            row_rect(body, 0),
            "Open when I hover the notch",
            g.expand_on_hover,
            "general",
            "expand_on_hover",
        );
        toggle_row(
            ui,
            row_rect(body, 1),
            "Hide for fullscreen apps",
            g.hide_in_fullscreen,
            "general",
            "hide_in_fullscreen",
        );
        toggle_row(ui, row_rect(body, 10), "Show bubbles beside the notch", g.bubbles, "general", "bubbles");
        toggle_row(ui, row_rect(body, 11), "Quiet sound with bubbles", g.bubble_sounds, "general", "bubble_sounds");
        toggle_row(ui, row_rect(body, 12), "Round dial for volume and brightness", g.hud_ring, "general", "hud_ring");
        toggle_row(ui, row_rect(body, 13), "Round dials fill clockwise", g.hud_clockwise, "general", "hud_clockwise");
        toggle_row(ui, row_rect(body, 2), "Start with Windows", g.start_with_windows, "general", "start_with_windows");

        let row = row_rect(body, 3);
        label(ui, row, "When idle");
        let sel = ["notch", "pill", "hidden"].iter().position(|s| *s == g.idle_style);
        let seg = Rect::new(row.right() - 232.0, row.y + 3.0, 224.0, row.h - 6.0);
        if let Some(i) = segmented(ui, seg, &["Notch", "Pill", "Hidden"], sel, accent) {
            put(ui, "general", "idle_style", toml_str(["notch", "pill", "hidden"][i]));
        }

        let row = row_rect(body, 4);
        label(ui, row, "Show on");
        let sel = match g.monitor.as_str() {
            "primary" => Some(0),
            "active" => Some(1),
            _ => None,
        };
        let seg = Rect::new(row.right() - 232.0, row.y + 3.0, 224.0, row.h - 6.0);
        if let Some(i) = segmented(ui, seg, &["Main screen", "Active window"], sel, accent) {
            put(ui, "general", "monitor", toml_str(["primary", "active"][i]));
        }

        toggle_row(ui, row_rect(body, 5), "Greet me when I unlock", self.cfg.modules.greeting, "modules", "greeting");

        // the name the unlock animation says hello to
        let row = row_rect(body, 6);
        label(ui, row, "Say hello to");
        let save = Rect::new(row.right() - 70.0, row.y + 2.0, 64.0, row.h - 4.0);
        let field = Rect::new(save.x - 8.0 - 190.0, row.y + 2.0, 190.0, row.h - 4.0);
        if self.name.draw(ui, field, "Your name", accent, self.focus == Focus::Name) {
            self.focus = Focus::Name;
            ui.fx.keyboard_focus = Some(true);
        }
        if ui.pill_button(save, "Save", None, ButtonStyle::inverted()) {
            let v = self.name.text().trim().to_string();
            put(ui, "greeting", "name", toml_str(&v));
            self.say("Name saved", true);
            self.focus = Focus::None;
            ui.fx.keyboard_focus = Some(false);
        }

        // the handwriting the unlock hello is written in
        let row = row_rect(body, 7);
        label(ui, row, "Hello font");
        let cur = self.cfg.greeting.font_name();
        let sel = if cur == "Yellowtail" {
            Some(0)
        } else if cur == "Script Regular" {
            Some(1)
        } else {
            None
        };
        let seg = Rect::new(row.right() - 232.0, row.y + 3.0, 224.0, row.h - 6.0);
        if let Some(i) = segmented(ui, seg, &["Yellowtail", "Script Regular"], sel, accent) {
            put(ui, "greeting", "font", toml_str(["Yellowtail", "Script Regular"][i]));
        }

        let row = row_rect(body, 8);
        let w = (row.w - 16.0) / 3.0;
        let style = ButtonStyle::filled(Color::white(0.08)).fg(palette::TEXT_DIM);
        let b = |k: f32| Rect::new(row.x + k * (w + 8.0), row.y + 2.0, w, row.h - 4.0);
        if ui.pill_button(b(0.0), "Edit config file", Some(Icon::File), style) {
            let path = config::config_path();
            if !path.exists() {
                let _ = std::fs::write(&path, config::DEFAULT_TEMPLATE);
            }
            shell_open(&path.to_string_lossy());
        }
        if ui.pill_button(b(1.0), "Open data folder", Some(Icon::Folder), style) {
            shell_open(&app_dir().to_string_lossy());
        }
        if ui.pill_button(b(2.0), "Reset home", Some(Icon::Refresh), style) {
            let d = crate::config::HomeCfg::default();
            put(ui, "home", "widgets", list_value(&d.widgets));
        }

        // updates: only ever checked when this button is pressed
        let row = row_rect(body, 9);
        label(ui, row, "Updates");
        let btn = Rect::new(row.right() - 150.0, row.y + 2.0, 142.0, row.h - 4.0);
        let (note, action) = match &self.update {
            Update::Idle => (format!("Version {}", updater::VERSION), "Check for updates".to_string()),
            Update::Checking => ("Looking…".to_string(), String::new()),
            Update::UpToDate => ("You're up to date".to_string(), "Check again".to_string()),
            Update::Available { tag, installer, notes, .. } => {
                let verb = if installer.is_some() { "Install" } else { "Open page" };
                // the first real line of the release notes: what's new
                let line = notes
                    .lines()
                    .map(|l| l.trim().trim_start_matches(['#', '-', '*', ' ']).trim())
                    .find(|l| !l.is_empty())
                    .unwrap_or("");
                let short: String = if line.chars().count() > 44 {
                    line.chars().take(43).collect::<String>() + "…"
                } else {
                    line.to_string()
                };
                let what = if short.is_empty() { format!("{tag} is available") } else { format!("{tag}: {short}") };
                (what, verb.to_string())
            }
            Update::Installing => ("Installing, the notch will restart".to_string(), String::new()),
            Update::Failed(why) => (why.clone(), "Try again".to_string()),
        };
        ui.p.text(
            &note,
            Rect::new(row.x + 90.0, row.y, (btn.x - row.x - 100.0).max(40.0), row.h),
            &TextStyle::new(11.5, palette::TEXT_DIM).right(),
        );
        if !action.is_empty() && ui.pill_button(btn, &action, None, ButtonStyle::inverted()) {
            match &self.update {
                Update::Available { installer: Some(url), .. } => {
                    updater::install(url, ui.bus);
                    self.update = Update::Installing;
                }
                Update::Available { page, .. } => {
                    shell_open(page);
                }
                _ => {
                    updater::check(&self.cfg.update.repo, ui.bus);
                    self.update = Update::Checking;
                }
            }
        }
    }

    fn look(&mut self, ui: &mut Ui, body: Rect) {
        let a = self.cfg.appearance.clone();
        let accent = ui.accent;
        toggle_row(ui, row_rect(body, 0), "Frosted glass", a.blur, "appearance", "blur");
        toggle_row(ui, row_rect(body, 1), "Shadow", a.shadow, "appearance", "shadow");
        slider_row(
            ui,
            row_rect(body, 2),
            "Panel opacity",
            a.expanded_opacity,
            (0.4, 1.0),
            format!("{:.0}%", a.expanded_opacity * 100.0),
            "appearance",
            "expanded_opacity",
        );
        slider_row(
            ui,
            row_rect(body, 3),
            "Panel width",
            a.expanded_width,
            (520.0, 960.0),
            format!("{:.0}", a.expanded_width),
            "appearance",
            "expanded_width",
        );
        slider_row(
            ui,
            row_rect(body, 4),
            "Size",
            a.scale,
            (0.8, 1.5),
            format!("{:.0}%", a.scale * 100.0),
            "appearance",
            "scale",
        );
        slider_row(
            ui,
            row_rect(body, 5),
            "Bounce",
            a.bounce,
            (0.0, 1.0),
            format!("{:.0}%", a.bounce * 100.0),
            "appearance",
            "bounce",
        );
        slider_row(
            ui,
            row_rect(body, 6),
            "Animation speed",
            a.animation_speed,
            (0.5, 2.0),
            format!("{:.1}×", a.animation_speed),
            "appearance",
            "animation_speed",
        );

        let row = row_rect(body, 7);
        label(ui, row, "Accent color");
        let auto_sel = a.accent.eq_ignore_ascii_case("auto");
        let mut x = row.right() - 12.0 - 6.0 * 26.0 - 64.0;
        let auto = Rect::new(x, row.y + 4.0, 56.0, row.h - 8.0);
        let style = if auto_sel {
            ButtonStyle::inverted()
        } else {
            ButtonStyle::filled(Color::white(0.08)).fg(palette::TEXT_DIM)
        };
        if ui.pill_button(auto, "Auto", None, style) {
            put(ui, "appearance", "accent", toml_str("auto"));
        }
        x += 64.0;
        for hex in SWATCHES {
            let c = Color::hex(hex);
            let cr = Rect::new(x, row.cy() - 10.0, 20.0, 20.0);
            let value = format!("#{hex:06X}");
            let sel = a.accent.eq_ignore_ascii_case(&value);
            if ui.hovered(cr) {
                ui.hot = true;
            }
            ui.p.fill_circle(cr.cx(), cr.cy(), 8.0, c);
            if sel {
                ui.p.stroke_circle(cr.cx(), cr.cy(), 10.5, 1.8, Color::white(0.95));
            }
            if ui.clicked(cr) {
                put(ui, "appearance", "accent", toml_str(&value));
            }
            x += 26.0;
        }

        // overall look: Tuna, Alcove or Nothing
        let row = row_rect(body, 8);
        label(ui, row, "Look");
        let sel = match a.look.trim().to_ascii_lowercase().as_str() {
            "alcove" => 1,
            "nothing" => 2,
            _ => 0,
        };
        let seg = Rect::new(row.right() - 296.0, row.y + 3.0, 288.0, row.h - 6.0);
        if let Some(i) = segmented(ui, seg, &["Tuna", "Alcove", "Nothing"], Some(sel), accent) {
            put(ui, "appearance", "look", toml_str(["tuna", "alcove", "nothing"][i]));
        }
        toggle_row(ui, row_rect(body, 9), "White beams (a hint of blue)", a.white_beams, "appearance", "white_beams");

        let row = row_rect(body, 10);
        label(ui, row, "Home clock");
        let sel = usize::from(a.clock_style.eq_ignore_ascii_case("flip"));
        let seg = Rect::new(row.right() - 232.0, row.y + 3.0, 224.0, row.h - 6.0);
        if let Some(i) = segmented(ui, seg, &["Rolling digits", "Flip clock"], Some(sel), accent) {
            put(ui, "appearance", "clock_style", toml_str(["roll", "flip"][i]));
        }
        toggle_row(
            ui,
            row_rect(body, 11),
            "Light animations (cached frames)",
            a.motion_cache,
            "appearance",
            "motion_cache",
        );
        toggle_row(
            ui,
            row_rect(body, 12),
            "Liquid glass: notch edge (bends what is behind)",
            a.glass_look,
            "appearance",
            "glass_look",
        );
        toggle_row(
            ui,
            row_rect(body, 13),
            "Live glass while media plays",
            a.glass_live,
            "appearance",
            "glass_live",
        );
        slider_row(
            ui,
            row_rect(body, 14),
            "Max frame rate",
            a.max_fps,
            (30.0, 240.0),
            format!("{:.0}", a.max_fps),
            "appearance",
            "max_fps",
        );
        toggle_row(
            ui,
            row_rect(body, 15),
            "Lighter animations (less CPU)",
            a.lite_motion,
            "appearance",
            "lite_motion",
        );
        toggle_row(ui, row_rect(body, 16), "Monochrome icons", a.mono_icons, "appearance", "mono_icons");
        toggle_row(ui, row_rect(body, 17), "Blur between pages (instead of sliding)", a.page_blur, "appearance", "page_blur");
        toggle_row(
            ui,
            row_rect(body, 18),
            "Launcher inside the notch (off = Tuna's launcher)",
            self.cfg.launcher.alcove,
            "launcher",
            "alcove",
        );
        for (i, (name, val, key)) in [
            ("Page switch time (higher = slower)", a.page_time, "page_time"),
            ("Notch close time (higher = slower)", a.close_time, "close_time"),
            ("Notch open time (higher = slower)", a.open_time, "open_time"),
        ]
        .into_iter()
        .enumerate()
        {
            slider_row(
                ui,
                row_rect(body, 19 + i),
                name,
                val,
                (0.4, 2.5),
                format!("{val:.1}×"),
                "appearance",
                key,
            );
        }
    }

    fn home(&mut self, ui: &mut Ui, body: Rect) {
        let accent = ui.accent;
        let chosen = self.cfg.home.widgets.clone();
        ui.p.text(
            &format!("Pick up to {MAX_HOME} widgets for the home page. Use the arrows below to change their order."),
            Rect::new(body.x + 2.0, body.y, body.w, 18.0),
            &TextStyle::new(12.0, palette::TEXT_DIM),
        );
        let cols = 4;
        let gap = 8.0;
        let cw = (body.w - gap * (cols as f32 - 1.0)) / cols as f32;
        let ch = 36.0;
        let top = body.y + 26.0;
        for (i, (id, name)) in HOME_CHOICES.iter().enumerate() {
            let (cx, cy) = (i % cols, i / cols);
            let cr = Rect::new(body.x + cx as f32 * (cw + gap), top + cy as f32 * (ch + gap), cw, ch);
            let pos = chosen.iter().position(|w| w == *id);
            if chip(ui, cr, name, pos.is_some(), pos.map(|p| p + 1), accent) {
                let mut next = chosen.clone();
                match pos {
                    Some(p) => {
                        next.remove(p);
                    }
                    None if next.len() < MAX_HOME => next.push(id.to_string()),
                    None => {
                        self.status = Some((format!("Only {MAX_HOME} fit. Remove one first."), Instant::now(), false));
                        continue;
                    }
                }
                put(ui, "home", "widgets", list_value(&next));
            }
        }
        // reorder what is chosen
        let oy = top + 2.0 * (ch + gap) + 6.0;
        if chosen.len() > 1 {
            ui.p.text(
                "Order",
                Rect::new(body.x + 2.0, oy, 200.0, 16.0),
                &TextStyle::new(11.5, palette::TEXT_FAINT).semibold(),
            );
            if let Some(next) = order_row(ui, Rect::new(body.x, oy + 20.0, body.w, 32.0), &chosen, &HOME_CHOICES, 4) {
                put(ui, "home", "widgets", list_value(&next));
            }
        }
    }

    fn modules(&mut self, ui: &mut Ui, body: Rect) {
        let accent = ui.accent;
        let cols = 4;
        let gap = 8.0;
        let cw = (body.w - gap * (cols as f32 - 1.0)) / cols as f32;
        let ch = 36.0;
        for (i, (id, name)) in MODULE_CHOICES.iter().enumerate() {
            let (cx, cy) = (i % cols, i / cols);
            let cr = Rect::new(body.x + cx as f32 * (cw + gap), body.y + cy as f32 * (ch + gap), cw, ch);
            let on = self.cfg.modules.is_enabled(id);
            if chip(ui, cr, name, on, None, accent) {
                put(ui, "modules", id, (!on).to_string());
            }
        }
    }

    fn weather(&mut self, ui: &mut Ui, body: Rect) {
        let accent = ui.accent;
        let row = row_rect(body, 0);
        label(ui, row, "City");
        let save = Rect::new(row.right() - 80.0, row.y + 2.0, 74.0, row.h - 4.0);
        let field = Rect::new(save.x - 8.0 - 250.0, row.y + 2.0, 250.0, row.h - 4.0);
        if self.city.draw(ui, field, "e.g. Berlin or Springfield, IL", accent, self.focus == Focus::City) {
            self.focus = Focus::City;
            ui.fx.keyboard_focus = Some(true);
        }
        if ui.pill_button(save, "Save", None, ButtonStyle::inverted()) {
            let v = self.city.text().trim().to_string();
            put(ui, "weather", "city", toml_str(&v));
            self.say("City saved", true);
            self.focus = Focus::None;
            ui.fx.keyboard_focus = Some(false);
        }

        let row = row_rect(body, 1);
        label(ui, row, "Units");
        let sel = if self.cfg.weather.units.eq_ignore_ascii_case("imperial") { 1 } else { 0 };
        let seg = Rect::new(row.right() - 232.0, row.y + 3.0, 224.0, row.h - 6.0);
        if let Some(i) = segmented(ui, seg, &["Metric °C", "Imperial °F"], Some(sel), accent) {
            put(ui, "weather", "units", toml_str(["metric", "imperial"][i]));
        }

        let row = row_rect(body, 2);
        ui.p.text(
            "Forecasts come from open-meteo.com. No account or key needed.",
            Rect::new(row.x + 2.0, row.y, row.w, row.h),
            &TextStyle::new(11.5, palette::TEXT_FAINT),
        );
    }

    fn notch(&mut self, ui: &mut Ui, body: Rect) {
        let accent = ui.accent;
        let chosen = self.cfg.notch.widgets.clone();
        ui.p.text(
            &format!(
                "Pick up to {MAX_NOTCH} widgets for the closed notch. Use the arrows below to change their order."
            ),
            Rect::new(body.x + 2.0, body.y, body.w, 18.0),
            &TextStyle::new(12.0, palette::TEXT_DIM),
        );
        let cols = 4;
        let gap = 8.0;
        let cw = (body.w - gap * (cols as f32 - 1.0)) / cols as f32;
        let ch = 36.0;
        let top = body.y + 26.0;
        for (i, (id, name)) in NOTCH_CHOICES.iter().enumerate() {
            let (cx, cy) = (i % cols, i / cols);
            let cr = Rect::new(body.x + cx as f32 * (cw + gap), top + cy as f32 * (ch + gap), cw, ch);
            let pos = chosen.iter().position(|w| w == *id);
            if chip(ui, cr, name, pos.is_some(), pos.map(|p| p + 1), accent) {
                let mut next = chosen.clone();
                match pos {
                    Some(p) => {
                        next.remove(p);
                    }
                    None if next.len() < MAX_NOTCH => next.push(id.to_string()),
                    None => {
                        self.status = Some((format!("Only {MAX_NOTCH} fit. Remove one first."), Instant::now(), false));
                        continue;
                    }
                }
                put(ui, "notch", "widgets", list_value(&next));
            }
        }
        // reorder what is chosen
        let oy = top + 2.0 * (ch + gap) + 6.0;
        if chosen.len() > 1 {
            ui.p.text(
                "Order",
                Rect::new(body.x + 2.0, oy, 200.0, 16.0),
                &TextStyle::new(11.5, palette::TEXT_FAINT).semibold(),
            );
            if let Some(next) = order_row(ui, Rect::new(body.x, oy + 20.0, body.w, 32.0), &chosen, &NOTCH_CHOICES, 4) {
                put(ui, "notch", "widgets", list_value(&next));
            }
        }
        ui.p.text(
            "They show while nothing is playing. With the pill style they appear when you hover it.",
            Rect::new(body.x + 2.0, oy + 58.0, body.w, 18.0),
            &TextStyle::new(11.5, palette::TEXT_FAINT),
        );
    }

    /// Settings → Glass: one slider for each dial of the Alcove glass edge.
    fn dock(&mut self, ui: &mut Ui, body: Rect) {
        let d = self.cfg.dock.clone();
        toggle_row(ui, row_rect(body, 0), "Show the dock", d.enabled, "dock", "enabled");
        toggle_row(ui, row_rect(body, 1), "Auto-hide", d.auto_hide, "dock", "auto_hide");
        toggle_row(ui, row_rect(body, 2), "Hide the Windows taskbar", d.hide_taskbar, "dock", "hide_taskbar");
        toggle_row(ui, row_rect(body, 3), "Show open apps", d.show_running, "dock", "show_running");
        slider_row(
            ui,
            row_rect(body, 4),
            "Icon size",
            d.icon_size,
            (32.0, 96.0),
            format!("{:.0}", d.icon_size),
            "dock",
            "icon_size",
        );
        slider_row(
            ui,
            row_rect(body, 5),
            "Magnification",
            d.magnify,
            (1.0, 2.2),
            format!("{:.1}x", d.magnify),
            "dock",
            "magnify",
        );
        slider_row(
            ui,
            row_rect(body, 6),
            "Dock darkness (higher = darker, more solid)",
            d.opacity,
            (0.1, 1.0),
            format!("{:.0}%", d.opacity * 100.0),
            "dock",
            "opacity",
        );
        slider_row(
            ui,
            row_rect(body, 7),
            "Distance from the bottom",
            d.bottom_margin,
            (0.0, 40.0),
            format!("{:.0}", d.bottom_margin),
            "dock",
            "bottom_margin",
        );
        slider_row(
            ui,
            row_rect(body, 8),
            "Rim thickness",
            d.rim_width,
            (0.0, 4.0),
            format!("{:.1}", d.rim_width),
            "dock",
            "rim_width",
        );
        slider_row(
            ui,
            row_rect(body, 9),
            "Rim brightness",
            d.rim_bright,
            (0.0, 1.0),
            format!("{:.0}%", d.rim_bright * 100.0),
            "dock",
            "rim_bright",
        );
        toggle_row(ui, row_rect(body, 10), "Melt into the screen edge", d.melt, "dock", "melt");
        let l = self.cfg.launcher.clone();
        slider_row(
            ui,
            row_rect(body, 11),
            "Launcher blur",
            l.blur,
            (0.0, 1.0),
            format!("{:.0}%", l.blur * 100.0),
            "launcher",
            "blur",
        );
        slider_row(
            ui,
            row_rect(body, 12),
            "Launcher darkness",
            l.dark,
            (0.2, 0.95),
            format!("{:.0}%", l.dark * 100.0),
            "launcher",
            "dark",
        );
        toggle_row(ui, row_rect(body, 13), "Launcher animations", l.animate, "launcher", "animate");
        toggle_row(ui, row_rect(body, 14), "Launcher light look (off = dark)", l.light, "launcher", "light");
        slider_row(
            ui,
            row_rect(body, 15),
            "Launcher size",
            l.size,
            (0.5, 1.5),
            format!("{:.0}%", l.size * 100.0),
            "launcher",
            "size",
        );
        toggle_row(ui, row_rect(body, 16), "Liquid glass: dock", d.glass, "dock", "glass");
        toggle_row(ui, row_rect(body, 17), "Liquid glass: launcher", l.glass, "launcher", "glass");
        toggle_row(ui, row_rect(body, 18), "Launcher: live blur (hides it from screenshots)", l.live, "launcher", "live");
        toggle_row(ui, row_rect(body, 20), "Launcher: one box (off = two boxes)", l.single_box, "launcher", "single_box");
        toggle_row(ui, row_rect(body, 21), "Launcher: the single box is a square", l.square_box, "launcher", "square_box");
        slider_row(
            ui,
            row_rect(body, 22),
            "Dock blur (0 = clear glass)",
            d.blur,
            (0.0, 1.0),
            format!("{:.0}%", d.blur * 100.0),
            "dock",
            "blur",
        );
        toggle_row(
            ui,
            row_rect(body, 23),
            "Dock: bending glass edge (hides it from screenshots)",
            d.refract,
            "dock",
            "refract",
        );
        slider_row(
            ui,
            row_rect(body, 19),
            "Launcher tint (dark look)",
            l.tint,
            (0.0, 0.6),
            format!("{:.0}%", l.tint * 100.0),
            "launcher",
            "tint",
        );
    }

    fn glass(&mut self, ui: &mut Ui, body: Rect) {
        let g = self.cfg.glass.clone();
        // (label, value, range, shown, key)
        let rows: [(&str, f32, (f32, f32), String, &str); 22] = [
            ("Edge width", g.edge_width, (20.0, 160.0), format!("{:.0}", g.edge_width), "edge_width"),
            ("Warp strength", g.pull, (0.0, 60.0), format!("{:.0}", g.pull), "pull"),
            ("Bend curve", g.bend, (0.8, 3.0), format!("{:.1}", g.bend), "bend"),
            ("Edge opacity", g.strength, (0.3, 1.0), format!("{:.0}%", g.strength * 100.0), "strength"),
            ("Corner warp", g.corner, (0.0, 1.0), format!("{:.0}%", g.corner * 100.0), "corner"),
            ("Colour fringing", g.fringe, (0.0, 0.2), format!("{:.0}%", g.fringe * 500.0), "fringe"),
            ("Outer edge softness", g.softness, (0.5, 12.0), format!("{:.1}", g.softness), "softness"),
            ("Middle glass", g.middle, (0.0, 1.0), format!("{:.0}%", g.middle * 100.0), "middle"),
            ("Middle warp", g.middle_warp, (0.0, 1.0), format!("{:.0}%", g.middle_warp * 100.0), "middle_warp"),
            ("Middle blur", g.middle_blur, (0.0, 6.0), format!("{:.1}", g.middle_blur), "middle_blur"),
            ("Bottom fade amount", g.fade_amount, (0.0, 1.0), format!("{:.0}%", g.fade_amount * 100.0), "fade_amount"),
            ("Bottom fade start", g.fade_start, (0.5, 3.0), format!("{:.1}", g.fade_start), "fade_start"),
            ("Glow fade-in speed", g.glow_in, (2.0, 30.0), format!("{:.0}", g.glow_in), "glow_in"),
            ("Live refresh (ms)", g.live_ms, (80.0, 500.0), format!("{:.0}", g.live_ms), "live_ms"),
            ("Live melt time (s)", g.melt, (0.03, 0.5), format!("{:.2}", g.melt), "melt"),
            ("Sharpen when left", g.sharpen_at, (0.02, 0.5), format!("{:.0}%", g.sharpen_at * 100.0), "sharpen_at"),
            (
                "Full-detail zone",
                g.sharp_zone,
                (0.3, 4.0),
                if g.sharp_zone >= 3.95 { "Everywhere".to_string() } else { format!("{:.1}x", g.sharp_zone) },
                "sharp_zone",
            ),
            ("Edge highlight", g.edge_light, (0.0, 1.0), format!("{:.0}%", g.edge_light * 100.0), "edge_light"),
            ("Frost tint", g.tint, (0.0, 1.0), format!("{:.0}%", g.tint * 100.0), "tint"),
            ("Readability dim", g.readable, (0.0, 1.0), format!("{:.0}%", g.readable * 100.0), "readable"),
            ("Stretch lean", g.fan, (0.0, 1.2), format!("{:.2}", g.fan), "fan"),
            ("Stretch length", g.stretch_len, (0.5, 1.6), format!("{:.2}x", g.stretch_len), "stretch_len"),
        ];
        for (i, (text, val, range, shown, key)) in rows.into_iter().enumerate() {
            slider_row(ui, row_rect(body, i), text, val, range, shown, "glass", key);
        }
        let d = crate::config::GlassCfg::default();
        toggle_row(ui, row_rect(body, 22), "Stretch edge (iOS look)", g.stretch, "glass", "stretch");
        toggle_row(ui, row_rect(body, 24), "Real glass bend (Snell's law)", g.snell, "glass", "snell");
        toggle_row(ui, row_rect(body, 31), "Press glow (Alcove)", g.glow, "glass", "glow");
        let amt = ("Press glow strength", g.glow_amount, (0.0, 1.0), format!("{:.0}%", g.glow_amount * 100.0));
        slider_row(ui, row_rect(body, 32), amt.0, amt.1, amt.2, amt.3, "glass", "glow_amount");
        let snell_rows: [(&str, f32, (f32, f32), String, &str); 3] = [
            ("Snell: glass index", g.snell_ior, (1.05, 2.4), format!("{:.2}", g.snell_ior), "snell_ior"),
            ("Snell: edge thickness", g.snell_thick, (0.3, 4.0), format!("{:.1}", g.snell_thick), "snell_thick"),
            ("Snell: edge roundness", g.snell_round, (2.0, 6.0), format!("{:.1}", g.snell_round), "snell_round"),
        ];
        for (i, (text, val, range, shown, key)) in snell_rows.into_iter().enumerate() {
            slider_row(ui, row_rect(body, 25 + i), text, val, range, shown, "glass", key);
        }
        let light_rows: [(&str, f32, (f32, f32), String, &str); 3] = [
            ("Rim light direction", g.light_angle, (0.0, 360.0), format!("{:.0}°", g.light_angle), "light_angle"),
            ("Follows the light", g.light_dir, (0.0, 1.0), format!("{:.0}%", g.light_dir * 100.0), "light_dir"),
            ("Rim light width", g.light_width, (0.8, 6.0), format!("{:.1}", g.light_width), "light_width"),
        ];
        for (i, (text, val, range, shown, key)) in light_rows.into_iter().enumerate() {
            slider_row(ui, row_rect(body, 28 + i), text, val, range, shown, "glass", key);
        }
        let b = row_rect(body, 23);
        let style = ButtonStyle::filled(Color::white(0.07)).fg(palette::TEXT_DIM);
        if ui.pill_button(Rect::new(b.x, b.y, 190.0, b.h), "Reset to defaults", None, style) {
            for (key, v) in [
                ("edge_width", d.edge_width),
                ("pull", d.pull),
                ("bend", d.bend),
                ("strength", d.strength),
                ("corner", d.corner),
                ("fringe", d.fringe),
                ("softness", d.softness),
                ("middle", d.middle),
                ("middle_warp", d.middle_warp),
                ("middle_blur", d.middle_blur),
                ("fade_amount", d.fade_amount),
                ("fade_start", d.fade_start),
                ("glow_in", d.glow_in),
                ("live_ms", d.live_ms),
                ("melt", d.melt),
                ("sharpen_at", d.sharpen_at),
                ("sharp_zone", d.sharp_zone),
                ("edge_light", d.edge_light),
                ("tint", d.tint),
                ("readable", d.readable),
                ("fan", d.fan),
                ("stretch_len", d.stretch_len),
                ("snell_ior", d.snell_ior),
                ("snell_thick", d.snell_thick),
                ("snell_round", d.snell_round),
                ("light_angle", d.light_angle),
                ("light_dir", d.light_dir),
                ("light_width", d.light_width),
                ("glow_amount", d.glow_amount),
            ] {
                put(ui, "glass", key, format!("{v:.2}"));
            }
        }
    }

    fn tabs(&mut self, ui: &mut Ui, body: Rect) {
        let order = tab_order(&self.cfg.tabs.order);
        ui.p.text(
            "Use the arrows to change the order of the tabs in the header. Home always stays first.",
            Rect::new(body.x + 2.0, body.y, body.w, 18.0),
            &TextStyle::new(12.0, palette::TEXT_DIM),
        );
        let top = body.y + 28.0;
        if let Some(next) = order_row(ui, Rect::new(body.x, top, body.w, 32.0), &order, &TAB_CHOICES, 4) {
            put(ui, "tabs", "order", list_value(&next));
        }
        let rows = order.len().div_ceil(4) as f32;
        let by = top + rows * 40.0 + 6.0;
        let style = ButtonStyle::filled(Color::white(0.08)).fg(palette::TEXT_DIM);
        if ui.pill_button(Rect::new(body.x, by, 170.0, 30.0), "Reset order", Some(Icon::Refresh), style) {
            put(ui, "tabs", "order", "[]".to_string());
        }
        ui.p.text(
            "Tabs for modules you switched off are hidden.",
            Rect::new(body.x + 182.0, by, body.w - 182.0, 30.0),
            &TextStyle::new(11.5, palette::TEXT_FAINT),
        );
    }

    /// The keys were recorded (or the recording was cancelled with Esc).
    fn apply_keys(&mut self, mods: u32, vk: u32) {
        self.recording = false;
        if vk == 0x1B && mods == 0 {
            self.say("Recording cancelled", false);
            return;
        }
        match crate::lowkeys::describe(mods, vk) {
            Some(name) => match crate::hotkeys::check_combo(&name) {
                Ok(()) => {
                    self.combo_keys = name;
                    self.say("Keys recorded", true);
                }
                Err(e) => self.say(e, false),
            },
            None => self.say("That key can't be used here, try another one", false),
        }
    }

    /// Check what was entered and add the combo; returns the new list to save.
    fn add_combo(&mut self) -> Option<Vec<String>> {
        let keys = self.combo_keys.trim().to_string();
        if keys.is_empty() {
            self.say("Click the keys button and press the keys first", false);
            return None;
        }
        if let Err(e) = crate::hotkeys::check_combo(&keys) {
            self.say(e, false);
            return None;
        }
        let target = self.combo_target.text().trim().replace('|', " ");
        if target.is_empty() {
            self.say("Choose what it should open: the Apps menu, or type a folder or link", false);
            return None;
        }
        let mut items = self.cfg.combos.items.clone();
        // the same keys again replace the old combo
        items.retain(|s| !crate::hotkeys::split_item(s).0.eq_ignore_ascii_case(&keys));
        if items.len() >= 12 {
            self.say("That is the most combos (12)", false);
            return None;
        }
        let label = if target == self.combo_path { self.combo_label.replace('|', " ") } else { String::new() };
        items.push(if label.is_empty() { format!("{keys}|{target}") } else { format!("{keys}|{target}|{label}") });
        self.combo_keys.clear();
        self.combo_target.clear();
        self.combo_label.clear();
        self.combo_path.clear();
        self.say("Combo added", true);
        Some(items)
    }

    /// Find the installed apps in the background (the first time the menu is opened).
    fn load_apps(&mut self) {
        if self.apps_started {
            return;
        }
        self.apps_started = true;
        let slot = self.apps.clone();
        let _ = std::thread::Builder::new().name("combo-apps".into()).spawn(move || {
            let mut v = crate::dock::installed_apps();
            v.sort_by_key(|(n, _)| n.to_lowercase());
            v.dedup_by(|a, b| a.0.eq_ignore_ascii_case(&b.0));
            if let Ok(mut g) = slot.lock() {
                *g = Some(v);
            }
        });
    }

    fn combos(&mut self, ui: &mut Ui, body: Rect) {
        let accent = ui.accent;
        // keys that were just recorded
        if let Some((m, vk)) = crate::lowkeys::take_captured() {
            self.apply_keys(m, vk);
        }
        let rec = crate::lowkeys::capturing();
        if self.recording && !rec {
            // ran out of time
            self.recording = false;
            crate::lowkeys::cancel_capture();
        }
        if self.apps_started && !self.apps_loaded {
            match self.apps.lock().ok().and_then(|mut g| g.take()) {
                Some(v) => {
                    self.app_list = v;
                    self.apps_loaded = true;
                }
                None => ui.animate(),
            }
        }
        if rec {
            // keep drawing, so the end of the recording is noticed
            ui.animate();
        }

        let row = row_rect(body, 0);
        ui.p.text(
            "Your own hotkeys. Click the keys button, press the keys, then choose what to open.",
            Rect::new(row.x + 2.0, row.y, row.w, row.h),
            &TextStyle::new(11.5, palette::TEXT_DIM),
        );
        let row = row_rect(body, 1);
        let add = Rect::new(row.right() - 64.0, row.y + 2.0, 64.0, row.h - 4.0);
        let apps = Rect::new(add.x - 8.0 - 84.0, row.y + 2.0, 84.0, row.h - 4.0);
        let kb = Rect::new(row.x, row.y + 2.0, 176.0, row.h - 4.0);
        let tf = Rect::new(kb.right() + 8.0, row.y + 2.0, (apps.x - 8.0) - (kb.right() + 8.0), row.h - 4.0);
        let (text, style) = if rec {
            ("Press the keys…".to_string(), ButtonStyle::inverted())
        } else if self.combo_keys.is_empty() {
            ("Click, then press keys".to_string(), ButtonStyle::filled(Color::white(0.10)))
        } else {
            (self.combo_keys.clone(), ButtonStyle::filled(Color::white(0.10)))
        };
        if ui.pill_button(kb, &text, Some(Icon::Keyboard), style) {
            if rec {
                self.recording = false;
                crate::lowkeys::cancel_capture();
            } else {
                self.recording = true;
                self.focus = Focus::None;
                ui.fx.keyboard_focus = Some(false);
                crate::lowkeys::start_capture(self.hwnd.hwnd());
            }
        }
        if self.combo_target.draw(ui, tf, "Apps menu, or a folder or link", accent, self.focus == Focus::ComboTarget) {
            self.focus = Focus::ComboTarget;
            ui.fx.keyboard_focus = Some(true);
        }
        if ui.pill_button(apps, "Apps", Some(Icon::Grid), ButtonStyle::filled(Color::white(0.10))) {
            self.picker = !self.picker;
            self.focus = Focus::None;
            ui.fx.keyboard_focus = Some(false);
            if self.picker {
                self.load_apps();
            }
        }
        if ui.pill_button(add, "Add", None, ButtonStyle::inverted()) {
            if let Some(list) = self.add_combo() {
                put(ui, "combos", "items", list_value(&list));
            }
            self.focus = Focus::None;
            ui.fx.keyboard_focus = Some(false);
        }

        if self.picker {
            self.picker_menu(ui, body, accent);
            return;
        }
        let items = self.cfg.combos.items.clone();
        if items.is_empty() {
            let row = row_rect(body, 2);
            ui.p.text(
                "No combos yet. Example: Win+B opens your default browser.",
                Rect::new(row.x + 2.0, row.y, row.w, row.h),
                &TextStyle::new(11.5, palette::TEXT_FAINT),
            );
        }
        for (i, item) in items.iter().enumerate().take(12) {
            let row = row_rect(body, 2 + i);
            let (keys, target, label) = crate::hotkeys::split_item(item);
            label_row(ui, row, keys);
            let x = row.x + 170.0;
            let shown = if label.is_empty() { target } else { label };
            ui.p.text(
                &crate::util::one_line(shown),
                Rect::new(x, row.y, row.right() - x - 48.0, row.h),
                &TextStyle::new(12.0, palette::TEXT_DIM),
            );
            let rm = Rect::new(row.right() - 36.0, row.y + 4.0, 28.0, row.h - 8.0);
            if ui.icon_button(rm, Icon::Close, ButtonStyle::filled(Color::white(0.08)).fg(palette::TEXT_DIM)) {
                let mut next = items.clone();
                next.remove(i);
                put(ui, "combos", "items", list_value(&next));
            }
        }
    }

    /// The menu of all apps (and the default browser): search, scroll, click one.
    fn picker_menu(&mut self, ui: &mut Ui, body: Rect, accent: Color) {
        let row = row_rect(body, 2);
        let close = Rect::new(row.right() - 84.0, row.y + 2.0, 84.0, row.h - 4.0);
        let find = Rect::new(row.x, row.y + 2.0, close.x - 8.0 - row.x, row.h - 4.0);
        if self.combo_find.draw(ui, find, "Search your apps", accent, self.focus == Focus::ComboFind) {
            self.focus = Focus::ComboFind;
            ui.fx.keyboard_focus = Some(true);
        }
        if ui.pill_button(
            close,
            "Close",
            Some(Icon::Close),
            ButtonStyle::filled(Color::white(0.08)).fg(palette::TEXT_DIM),
        ) {
            self.picker = false;
            self.radial_slot = None;
            self.focus = Focus::None;
            ui.fx.keyboard_focus = Some(false);
            return;
        }
        let q = self.combo_find.text().trim().to_lowercase();
        let mut entries: Vec<(&str, &str)> = Vec::new();
        if q.is_empty() || "default browser".contains(&q) {
            entries.push(("Default browser", "browser"));
        }
        entries.extend(
            self.app_list
                .iter()
                .filter(|(n, _)| q.is_empty() || n.to_lowercase().contains(&q))
                .map(|(n, p)| (n.as_str(), p.as_str())),
        );
        let pitch = ROW_H + ROW_GAP;
        let area = Rect::new(body.x, row_rect(body, 3).y, body.w, 8.0 * pitch - ROW_GAP);
        let content_h = entries.len() as f32 * pitch;
        let off = crate::alcove::scroll_rows(ui, area, &mut self.pick_scroll, content_h, pitch, "combo-pick");
        let was_live = ui.interactive;
        // rows scrolled out of view must not answer the pointer
        ui.interactive = was_live && ui.input.mouse.is_some_and(|(x, y)| area.contains(x, y));
        ui.p.push_clip(area);
        let mut chosen: Option<(String, String)> = None;
        for (i, (name, path)) in entries.iter().enumerate() {
            let r = Rect::new(area.x, area.y - off + i as f32 * pitch, area.w, ROW_H);
            if r.bottom() < area.y || r.y > area.bottom() {
                continue;
            }
            let hover = ui.hovered(r);
            ui.row_bg(r, hover, None);
            ui.p.text(name, Rect::new(r.x + 12.0, r.y, r.w - 24.0, r.h), &TextStyle::new(13.0, palette::TEXT));
            if ui.clicked(r) {
                chosen = Some((name.to_string(), path.to_string()));
            }
        }
        if !self.apps_loaded {
            let r = Rect::new(area.x, area.y + pitch, area.w, ROW_H);
            ui.p.text(
                "Loading your apps…",
                Rect::new(r.x + 12.0, r.y, r.w, r.h),
                &TextStyle::new(12.0, palette::TEXT_FAINT),
            );
        }
        ui.p.pop_clip();
        ui.interactive = was_live;
        if let Some((name, path)) = chosen {
            if let Some(slot) = self.radial_slot.take() {
                // an app for a slice of the radial menu
                let name = if path == "browser" { "Browser".to_string() } else { name.replace('|', " ") };
                let mut apps = self.cfg.radial.apps.clone();
                apps.resize(16, String::new());
                if let Some(a) = apps.get_mut(slot) {
                    *a = format!("{name}|{}", path.replace('|', " "));
                }
                put(ui, "radial", "apps", list_value(&apps));
                self.picker = false;
                self.focus = Focus::None;
                ui.fx.keyboard_focus = Some(false);
                return;
            }
            self.combo_path = path.replace('|', " ");
            self.combo_label = if path == "browser" { String::new() } else { name };
            let shown = self.combo_path.clone();
            self.combo_target.set(&shown);
            self.picker = false;
            self.focus = Focus::None;
            ui.fx.keyboard_focus = Some(false);
        }
    }

    /// Settings → Radial: the radial menu (Win+M): on or off, the taskbar option and the 16 apps.
    fn radial(&mut self, ui: &mut Ui, body: Rect) {
        let accent = ui.accent;
        if self.apps_started && !self.apps_loaded {
            match self.apps.lock().ok().and_then(|mut g| g.take()) {
                Some(v) => {
                    self.app_list = v;
                    self.apps_loaded = true;
                }
                None => ui.animate(),
            }
        }
        if self.picker && self.radial_slot.is_some() {
            self.picker_menu(ui, body, accent);
            return;
        }
        let r = self.cfg.radial.clone();
        toggle_row(ui, row_rect(body, 0), "Radial menu (Win+M)", r.enabled, "radial", "enabled");
        toggle_row(ui, row_rect(body, 1), "Hide the Windows taskbar", r.hide_taskbar, "radial", "hide_taskbar");
        toggle_row(ui, row_rect(body, 2), "Tuna dark look", r.tuna, "radial", "tuna");
        slider_row(
            ui,
            row_rect(body, 3),
            "Glass blur",
            r.blur,
            (0.0, 3.0),
            format!("{:.1}", r.blur),
            "radial",
            "blur",
        );
        slider_row(
            ui,
            row_rect(body, 4),
            "Glassiness",
            r.glass,
            (0.0, 1.0),
            format!("{:.0}%", r.glass * 100.0),
            "radial",
            "glass",
        );
        let row = row_rect(body, 5);
        ui.p.text(
            "Pick an app for each slice. The inner ring comes first; both run clockwise from the top.",
            Rect::new(row.x + 2.0, row.y, row.w, row.h),
            &TextStyle::new(11.5, palette::TEXT_DIM),
        );
        const POS: [&str; 8] =
            ["top", "top right", "right", "bottom right", "bottom", "bottom left", "left", "top left"];
        let mut apps = r.apps.clone();
        apps.resize(16, String::new());
        for i in 0..16 {
            let row = row_rect(body, 6 + i);
            label_row(ui, row, &format!("{} \u{b7} {}", if i < 8 { "Inner" } else { "Outer" }, POS[i % 8]));
            let item = apps[i].trim();
            let name = item.split_once('|').map_or(item, |(n, _)| n.trim());
            let choose = Rect::new(row.right() - 36.0 - 8.0 - 96.0, row.y + 2.0, 96.0, row.h - 4.0);
            let x = row.x + 190.0;
            let (shown, color) = if item.is_empty() {
                ("Empty", palette::TEXT_FAINT)
            } else if name.is_empty() {
                (item, palette::TEXT_DIM)
            } else {
                (name, palette::TEXT_DIM)
            };
            ui.p.text(
                &crate::util::one_line(shown),
                Rect::new(x, row.y, (choose.x - 8.0 - x).max(10.0), row.h),
                &TextStyle::new(12.0, color),
            );
            if ui.pill_button(choose, "Choose", Some(Icon::Grid), ButtonStyle::filled(Color::white(0.10))) {
                self.radial_slot = Some(i);
                self.picker = true;
                self.scroll = 0.0;
                self.pick_scroll = 0.0;
                self.focus = Focus::None;
                ui.fx.keyboard_focus = Some(false);
                self.load_apps();
            }
            if !item.is_empty() {
                let rm = Rect::new(row.right() - 36.0, row.y + 4.0, 28.0, row.h - 8.0);
                if ui.icon_button(rm, Icon::Close, ButtonStyle::filled(Color::white(0.08)).fg(palette::TEXT_DIM)) {
                    let mut next = apps.clone();
                    next[i].clear();
                    put(ui, "radial", "apps", list_value(&next));
                }
            }
        }
    }

    fn gemini(&mut self, ui: &mut Ui, body: Rect) {
        let _accent = ui.accent;
        let blue = Color::hex(0x4285F4);

        let row = row_rect(body, 0);
        label(ui, row, "API key");
        let save = Rect::new(row.right() - 80.0, row.y + 2.0, 74.0, row.h - 4.0);
        let field = Rect::new(save.x - 8.0 - 250.0, row.y + 2.0, 250.0, row.h - 4.0);
        let hint = if self.has_key { "Saved. Paste a new key to replace it" } else { "Paste your key (AIza…)" };
        if self.key.draw(ui, field, hint, blue, self.focus == Focus::Key) {
            self.focus = Focus::Key;
            ui.fx.keyboard_focus = Some(true);
        }
        let can = !self.key.is_empty();
        let style =
            if can { ButtonStyle::inverted() } else { ButtonStyle::filled(Color::white(0.08)).fg(palette::TEXT_FAINT) };
        if ui.pill_button(save, "Save", None, style) && can {
            let bus = ui.bus.clone();
            self.save_key(&bus);
            self.focus = Focus::None;
            ui.fx.keyboard_focus = Some(false);
        }

        let row = row_rect(body, 1);
        label(ui, row, "Model");
        let save = Rect::new(row.right() - 80.0, row.y + 2.0, 74.0, row.h - 4.0);
        let field = Rect::new(save.x - 8.0 - 250.0, row.y + 2.0, 250.0, row.h - 4.0);
        if self.model.draw(ui, field, "gemini-2.5-flash", blue, self.focus == Focus::Model) {
            self.focus = Focus::Model;
            ui.fx.keyboard_focus = Some(true);
        }
        if ui.pill_button(save, "Save", None, ButtonStyle::inverted()) {
            let v = self.model.text().trim().to_string();
            if !v.is_empty() {
                put(ui, "gemini", "model", toml_str(&v));
                self.say("Model saved", true);
            }
            self.focus = Focus::None;
            ui.fx.keyboard_focus = Some(false);
        }

        let row = row_rect(body, 2);
        let style = ButtonStyle::filled(Color::white(0.08)).fg(palette::TEXT_DIM);
        if ui.pill_button(Rect::new(row.x, row.y + 2.0, 170.0, row.h - 4.0), "Get a free key", Some(Icon::Link), style)
        {
            shell_open("https://aistudio.google.com/apikey");
        }
        if self.has_key
            && ui.pill_button(
                Rect::new(row.x + 178.0, row.y + 2.0, 150.0, row.h - 4.0),
                "Remove saved key",
                Some(Icon::Trash),
                style,
            )
        {
            dotenv::set_saved(gemini::KEY_VAR, "");
            self.has_key = dotenv::get_saved_first(gemini::KEY_VAR).is_some();
            ui.bus.to_module("gemini", gemini::KeyChanged);
            self.say("Saved key removed", true);
        }

        let row = row_rect(body, 3);
        ui.p.text(
            "The key stays on this PC (in your DynamicNotch folder), never in config.toml.",
            Rect::new(row.x + 2.0, row.y, row.w, row.h),
            &TextStyle::new(11.5, palette::TEXT_FAINT),
        );
    }
}
