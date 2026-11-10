//! Gemini chat tab, separate from the OpenRouter-based "Ask AI".
//!
//! Paste your key right on the page (or in Settings → Gemini). It is saved in
//! `%APPDATA%\DynamicNotch\.env` as GEMINI_API_KEY, never in config.toml; a
//! GEMINI_API_KEY from the environment or another `.env` works too.
//! Answers stream in over SSE (`streamGenerateContent?alt=sse`).

use std::any::Any;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};

use super::widgets::TextField;
use super::{Cx, Key, Module, ModuleId, SystemEvent};
use crate::bus::Bus;
use crate::config::GeminiCfg;
use crate::gfx::{Icon, TextStyle};
use crate::sys::{clip, dotenv, http, shell_open};
use crate::ui::{gkey, ButtonStyle, Ui};
use crate::util::{palette, Color, Rect, SendHwnd};

const ID: ModuleId = "gemini";
pub const KEY_VAR: &str = "GEMINI_API_KEY";
/// The message line at the bottom: the text, with a rule of beams under it.
const INPUT_H: f32 = 38.0;
/// The conversation area grows with the chat between these heights, then scrolls.
const LOG_MIN_H: f32 = 96.0;
const LOG_MAX_H: f32 = 290.0;
const KEY_PAGE_H: f32 = 170.0;

/// Sent by the settings page after it saved or removed the API key.
pub struct KeyChanged;

/// A question from the Alcove launcher: it is asked at once.
pub struct Ask(pub String);

enum GMsg {
    Delta(u64, String),
    Done(u64),
    Error(u64, String),
}

#[derive(Clone)]
struct Turn {
    user: String,
    assistant: String,
}

pub struct Gemini {
    input: TextField,
    key_field: TextField,
    has_key: bool,
    /// Showing the key form even though a key exists (Change key).
    editing_key: bool,
    key_error: Option<String>,
    history: Vec<Turn>,
    current: Option<Turn>,
    streaming: bool,
    error: Option<String>,
    gen: u64,
    cancel: Option<Arc<AtomicBool>>,
    scroll: f32,
    stick_bottom: bool,
    text_h: f32,
    hwnd: SendHwnd,
    bus: Option<Bus>,
    cfg: GeminiCfg,
}

impl Gemini {
    pub fn new() -> Self {
        Self {
            input: TextField::new(4000).flat(),
            key_field: TextField::new(200).masked().flat(),
            has_key: false,
            editing_key: false,
            key_error: None,
            history: Vec::new(),
            current: None,
            streaming: false,
            error: None,
            gen: 0,
            cancel: None,
            scroll: 0.0,
            stick_bottom: true,
            text_h: 0.0,
            hwnd: SendHwnd(0),
            bus: None,
            cfg: GeminiCfg::default(),
        }
    }

    fn key_form(&self) -> bool {
        !self.has_key || self.editing_key
    }

    fn save_key(&mut self) -> bool {
        let key: String = self.key_field.text().chars().filter(|c| !c.is_whitespace()).collect();
        if key.len() < 20 {
            self.key_error = Some("That doesn't look like a Gemini API key".into());
            return false;
        }
        if !dotenv::set_saved(KEY_VAR, &key) {
            self.key_error = Some("Couldn't save the key (is %APPDATA% writable?)".into());
            return false;
        }
        self.key_field.clear();
        self.key_error = None;
        self.has_key = true;
        self.editing_key = false;
        true
    }

    fn stop_stream(&mut self) {
        if let Some(c) = self.cancel.take() {
            c.store(true, Ordering::Relaxed);
        }
        if self.streaming {
            self.streaming = false;
            if let Some(t) = self.current.take() {
                if !t.assistant.is_empty() {
                    self.history.push(t);
                }
            }
        }
    }

    fn new_chat(&mut self) {
        self.stop_stream();
        self.history.clear();
        self.current = None;
        self.error = None;
        self.scroll = 0.0;
        self.stick_bottom = true;
    }

    fn submit(&mut self) {
        let prompt = self.input.text().trim().to_string();
        if prompt.is_empty() || self.streaming {
            return;
        }
        let Some(key) = dotenv::get_saved_first(KEY_VAR) else {
            self.has_key = false;
            return;
        };
        let Some(bus) = self.bus.clone() else { return };
        if let Some(t) = self.current.take() {
            self.history.push(t);
        }
        self.error = None;
        self.input.clear();
        self.gen += 1;
        self.streaming = true;
        self.stick_bottom = true;

        let mut contents = Vec::new();
        for t in self.history.iter().rev().take(8).collect::<Vec<_>>().into_iter().rev() {
            contents.push(json!({ "role": "user", "parts": [{ "text": t.user }] }));
            contents.push(json!({ "role": "model", "parts": [{ "text": t.assistant }] }));
        }
        contents.push(json!({ "role": "user", "parts": [{ "text": prompt }] }));
        self.current = Some(Turn { user: prompt, assistant: String::new() });

        let body = json!({
            "systemInstruction": { "parts": [{ "text": self.cfg.system_prompt }] },
            "contents": contents,
            "generationConfig": { "maxOutputTokens": self.cfg.max_tokens },
        });
        let url = format!(
            "{}/models/{}:streamGenerateContent?alt=sse",
            self.cfg.endpoint.trim_end_matches('/'),
            self.cfg.model.trim()
        );
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        let gen = self.gen;
        let _ = std::thread::Builder::new()
            .name("gemini-request".into())
            .spawn(move || request(url, key, body, gen, cancel, bus));
    }

    fn response(&self) -> Option<&Turn> {
        self.current.as_ref().or(self.history.last())
    }
}

fn request(url: String, key: String, body: Value, gen: u64, cancel: Arc<AtomicBool>, bus: Bus) {
    let headers = [
        ("x-goog-api-key", key),
        ("Content-Type", "application/json".to_string()),
        ("Accept", "text/event-stream".to_string()),
    ];
    let payload = body.to_string().into_bytes();
    let mut parser = http::SseParser::default();
    let mut raw = Vec::new();
    let mut stream_error: Option<String> = None;
    let res = http::post_stream(&url, &headers, &payload, &cancel, |chunk| {
        if raw.len() < 16 * 1024 {
            raw.extend_from_slice(chunk);
        }
        parser.push(chunk, |data| {
            let Ok(v) = serde_json::from_str::<Value>(data) else { return };
            if let Some(e) = v.get("error") {
                stream_error = Some(e.get("message").and_then(Value::as_str).unwrap_or("stream error").to_string());
                return;
            }
            if let Some(parts) = v.pointer("/candidates/0/content/parts").and_then(Value::as_array) {
                let text: String = parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect();
                if !text.is_empty() {
                    bus.to_module(ID, GMsg::Delta(gen, text));
                }
            }
            if let Some(reason) = v.pointer("/promptFeedback/blockReason").and_then(Value::as_str) {
                stream_error = Some(format!("Gemini blocked the request ({reason})"));
            }
        });
        !cancel.load(Ordering::Relaxed)
    });
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    match res {
        Ok(200) if stream_error.is_none() => bus.to_module(ID, GMsg::Done(gen)),
        Ok(status) => {
            let msg = stream_error.unwrap_or_else(|| {
                serde_json::from_slice::<Value>(&raw)
                    .ok()
                    .and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_else(|| format!("HTTP {status}"))
            });
            let hint = match status {
                400 | 401 | 403 => " (check your API key in Settings → Gemini)",
                404 => " (check the model name in Settings → Gemini)",
                429 => " (rate limit or quota reached, try again shortly)",
                _ => "",
            };
            bus.to_module(ID, GMsg::Error(gen, format!("{msg}{hint}")));
        }
        Err(e) => bus.to_module(ID, GMsg::Error(gen, e)),
    }
}

impl Module for Gemini {
    fn id(&self) -> ModuleId {
        ID
    }
    fn title(&self) -> &str {
        "Gemini"
    }
    fn icon(&self) -> Icon {
        Icon::Chat
    }

    fn start(&mut self, cx: &mut Cx) {
        self.hwnd = cx.hwnd;
        self.bus = Some(cx.bus.clone());
        self.cfg = cx.cfg.gemini.clone();
        self.has_key = dotenv::get_saved_first(KEY_VAR).is_some();
    }

    fn stop(&mut self) {
        self.stop_stream();
    }

    fn on_message(&mut self, msg: Box<dyn Any + Send>, cx: &mut Cx) {
        let msg = match msg.downcast::<Ask>() {
            Ok(a) => {
                self.input.set(&a.0);
                if !self.key_form() {
                    self.submit();
                }
                cx.fx.layout_changed = true;
                cx.fx.redraw = true;
                return;
            }
            Err(m) => m,
        };
        let msg = match msg.downcast::<KeyChanged>() {
            Ok(_) => {
                self.has_key = dotenv::get_saved_first(KEY_VAR).is_some();
                self.editing_key = false;
                cx.fx.layout_changed = true;
                return;
            }
            Err(m) => m,
        };
        let Ok(m) = msg.downcast::<GMsg>() else { return };
        match *m {
            GMsg::Delta(g, t) if g == self.gen => {
                if let Some(cur) = self.current.as_mut() {
                    cur.assistant.push_str(&t);
                }
            }
            GMsg::Done(g) if g == self.gen => {
                self.streaming = false;
                self.cancel = None;
            }
            GMsg::Error(g, e) if g == self.gen => {
                self.streaming = false;
                self.cancel = None;
                self.error = Some(e);
                if self.current.as_ref().is_some_and(|c| c.assistant.is_empty()) {
                    if let Some(c) = self.current.take() {
                        self.input.set(&c.user);
                    }
                }
            }
            _ => {}
        }
        cx.fx.layout_changed = true;
    }

    fn on_system(&mut self, ev: &SystemEvent, cx: &mut Cx) {
        match ev {
            SystemEvent::ConfigReloaded => {
                self.cfg = cx.cfg.gemini.clone();
                // a key saved from the settings page
                self.has_key = dotenv::get_saved_first(KEY_VAR).is_some();
            }
            SystemEvent::Expanded(false) => self.editing_key = false,
            _ => {}
        }
    }

    fn has_page(&self) -> bool {
        true
    }

    fn wants_keyboard(&self) -> bool {
        true
    }

    fn page_height(&self, _w: f32) -> f32 {
        let al = crate::ui::look_alcove();
        if self.key_form() {
            return if al { 196.0 } else { KEY_PAGE_H };
        }
        self.text_h.clamp(LOG_MIN_H, LOG_MAX_H) + 12.0 + if al { 48.0 } else { INPUT_H }
    }

    fn on_key(&mut self, key: Key, cx: &mut Cx) {
        if self.key_form() {
            if self.key_field.key(key, self.hwnd) && self.save_key() {
                cx.fx.layout_changed = true;
            }
        } else {
            match key {
                Key::Up => {
                    self.scroll = (self.scroll - 40.0).max(0.0);
                    self.stick_bottom = false;
                }
                Key::Down => self.scroll += 40.0,
                Key::Copy => {
                    if let Some(t) = self.response() {
                        clip::set_text(self.hwnd.hwnd(), &t.assistant.clone());
                    }
                }
                _ => {
                    if self.input.key(key, self.hwnd) {
                        self.submit();
                    }
                }
            }
        }
        cx.fx.layout_changed = true;
    }

    fn draw_page(&mut self, ui: &mut Ui, r: Rect) {
        let al = crate::ui::look_alcove();
        if self.key_form() {
            if al {
                self.draw_key_form_alcove(ui, r);
            } else {
                self.draw_key_form(ui, r);
            }
        } else if al {
            self.draw_chat_alcove(ui, r);
        } else {
            self.draw_chat(ui, r);
        }
    }
}

impl Gemini {
    fn draw_key_form(&mut self, ui: &mut Ui, r: Rect) {
        ui.p.text(
            "Connect Gemini",
            Rect::new(r.x + 6.0, r.y, r.w - 12.0, 26.0),
            &TextStyle::new(18.0, palette::TEXT).semibold(),
        );
        ui.p.text(
            "Paste your API key. It stays on this PC, in your DynamicNotch folder.",
            Rect::new(r.x + 6.0, r.y + 28.0, r.w - 12.0, 18.0),
            &TextStyle::new(12.0, palette::TEXT_FAINT),
        );
        // one line to type on, with a rule of beams under it
        let row = Rect::new(r.x + 6.0, r.y + 64.0, r.w - 12.0, 30.0);
        let save = Rect::new(row.right() - 64.0, row.y, 64.0, 28.0);
        let field = Rect::new(row.x, row.y, row.w - 76.0, 28.0);
        if self.key_field.draw_bare(ui, field, "AIza…", true, 15.0) {
            ui.fx.keyboard_focus = Some(true);
        }
        let can = !self.key_field.is_empty();
        ui.ticks(Rect::new(row.x, row.bottom() + 2.0, row.w, 6.0), if can { 1.0 } else { 0.0 }, Color::white(0.9));
        let style = if can { ButtonStyle::inverted() } else { ButtonStyle::default().fg(palette::TEXT_FAINT) };
        if ui.pill_button(save, "Save", None, style) && can && self.save_key() {
            ui.fx.layout_changed = true;
        }
        if let Some(e) = &self.key_error {
            ui.p.text(e, Rect::new(row.x, row.bottom() + 14.0, row.w, 16.0), &TextStyle::new(12.0, palette::RED));
        }
        let quiet = ButtonStyle::default().fg(palette::TEXT_DIM);
        if ui.pill_button(
            Rect::new(r.x + 2.0, r.bottom() - 26.0, 150.0, 26.0),
            "Get a free key",
            Some(Icon::Link),
            quiet,
        ) {
            shell_open("https://aistudio.google.com/apikey");
        }
        if self.has_key
            && ui.pill_button(Rect::new(r.right() - 80.0, r.bottom() - 26.0, 78.0, 26.0), "Cancel", None, quiet)
        {
            self.editing_key = false;
            self.key_field.clear();
            ui.fx.layout_changed = true;
        }
    }

    /// Chat with no boxes at all: the conversation as plain text, and one line to
    /// type on. The rule of beams under that line is the only decoration.
    fn draw_chat(&mut self, ui: &mut Ui, r: Rect) {
        let input = Rect::new(r.x + 6.0, r.bottom() - INPUT_H, r.w - 12.0, INPUT_H);
        let log = Rect::new(r.x, r.y, r.w, (input.y - 12.0 - r.y).max(0.0));
        self.draw_log(ui, log);

        // icons at the right end of the line, right to left: send/stop, new chat, copy, key
        let sz = 28.0;
        let cy = input.y + 14.0;
        let mut x = input.right() - sz;
        let can = !self.input.is_empty();
        let at = |x: f32| Rect::new(x, cy - sz * 0.5, sz, sz);
        if self.streaming {
            if ui.icon_button(at(x), Icon::Stop, ButtonStyle::default().scale(0.55).fg(palette::TEXT)) {
                self.stop_stream();
            }
        } else {
            let fg = if can { palette::TEXT } else { palette::TEXT_FAINT };
            if ui.icon_button(at(x), Icon::Send, ButtonStyle::default().scale(0.55).fg(fg)) && can {
                self.submit();
            }
        }
        let quiet = ButtonStyle::default().scale(0.55).fg(Color::white(0.38));
        if self.response().is_some() {
            x -= sz;
            if ui.icon_button(at(x), Icon::Refresh, quiet) {
                self.new_chat();
                ui.fx.layout_changed = true;
            }
            if let Some(t) = self.response() {
                if !t.assistant.is_empty() && !self.streaming {
                    x -= sz;
                    let text = t.assistant.clone();
                    if ui.icon_button(at(x), Icon::Copy, quiet) {
                        clip::set_text(self.hwnd.hwnd(), &text);
                    }
                }
            }
        }
        x -= sz;
        if ui.icon_button(at(x), Icon::Lock, quiet) {
            self.editing_key = true;
            self.key_field.clear();
            ui.fx.layout_changed = true;
        }

        let hint = if self.streaming { "Answering…" } else { "Ask anything" };
        let field = Rect::new(input.x, input.y, (x - 6.0 - input.x).max(80.0), 28.0);
        if self.input.draw_bare(ui, field, hint, true, 15.0) {
            ui.fx.keyboard_focus = Some(true);
        }

        // the rule of beams under the line: sweeps while Gemini answers, fills as you type,
        // and otherwise shows how much of the chat Gemini can see
        let rail = Rect::new(input.x, input.bottom() - 7.0, input.w, 6.0);
        if self.streaming {
            ui.ticks_sweep(rail, Color::white(0.9));
        } else if can {
            // the beams end exactly under the last character you typed
            let text = self.input.text();
            let (tw, _) = ui.p.measure(&text, &TextStyle::new(15.0, palette::TEXT));
            let end = (tw + 2.0).min(field.w).max(0.0);
            let frac = ui.glide(gkey("gem-rail", 0), end / rail.w.max(1.0));
            ui.ticks(rail, frac, Color::white(0.9));
        } else {
            let turns = self.history.len() + usize::from(self.current.is_some());
            ui.ticks(rail, (turns as f32 / 8.0).min(1.0), Color::white(0.9));
        }
    }

    /// The conversation as plain text on black. What you asked is small and dim with a
    /// thin beam beside it; Gemini's answer is larger and bright. No bubbles, no dividers.
    /// Scrolls, and sticks to the newest message until you scroll up.
    fn draw_log(&mut self, ui: &mut Ui, area: Rect) {
        #[derive(PartialEq, Clone, Copy)]
        enum Kind {
            User,
            Bot,
            Error,
            Working,
        }

        let ask = TextStyle::new(12.5, palette::TEXT_DIM).wrap().top();
        let answer = TextStyle::new(14.5, palette::TEXT).wrap().top();
        let col_x = area.x + 16.0;
        let col_w = (area.w - 40.0).max(160.0);
        let gap_small = 6.0;
        let gap_turn = 20.0;

        // what to show, oldest first: (kind, which turn); the texts stay where they are
        let n_hist = self.history.len();
        let mut msgs: Vec<(Kind, usize)> = Vec::new();
        for i in 0..n_hist + usize::from(self.current.is_some()) {
            let t = if i < n_hist { &self.history[i] } else { self.current.as_ref().unwrap() };
            msgs.push((Kind::User, i));
            if !t.assistant.is_empty() {
                msgs.push((Kind::Bot, i));
            }
            if self.streaming && i >= n_hist {
                msgs.push((Kind::Working, i));
            }
        }
        if self.error.is_some() {
            msgs.push((Kind::Error, 0));
        }

        if msgs.is_empty() {
            self.text_h = 0.0;
            ui.p.text(
                "Ask anything",
                Rect::new(area.x, area.cy() - 22.0, area.w, 30.0),
                &TextStyle::new(22.0, Color::white(0.30)).semibold().display().center(),
            );
            ui.p.text(
                "Enter to send",
                Rect::new(area.x, area.cy() + 10.0, area.w, 18.0),
                &TextStyle::new(12.0, palette::TEXT_FAINT).center(),
            );
            return;
        }

        // heights (the layouts are cached, so this is cheap after the first frame)
        let mut heights: Vec<f32> = Vec::with_capacity(msgs.len());
        for &(kind, i) in &msgs {
            let h = match kind {
                Kind::User => {
                    let t = if i < n_hist { &self.history[i] } else { self.current.as_ref().unwrap() };
                    ui.p.measure_wrapped(&t.user, &ask, col_w - 14.0) + 2.0
                }
                Kind::Bot => {
                    let t = if i < n_hist { &self.history[i] } else { self.current.as_ref().unwrap() };
                    ui.p.measure_wrapped(&t.assistant, &answer, col_w) + 2.0
                }
                Kind::Error => ui.p.measure_wrapped(self.error.as_deref().unwrap_or(""), &answer, col_w) + 2.0,
                Kind::Working => 12.0,
            };
            heights.push(h);
        }
        // a question and its answer sit close; a new question starts a new turn
        let gap_after = |k: usize| match msgs.get(k + 1) {
            Some((Kind::User, _)) => gap_turn,
            Some(_) => gap_small,
            None => 0.0,
        };
        let total: f32 = heights.iter().enumerate().map(|(k, h)| h + gap_after(k)).sum();
        self.text_h = total + 12.0;

        let max_scroll = (total - area.h).max(0.0);
        let wheel = ui.wheel(area);
        if wheel != 0.0 {
            self.scroll -= wheel * 40.0;
            self.stick_bottom = self.scroll >= max_scroll - 1.0;
        }
        if self.stick_bottom {
            self.scroll = max_scroll;
        }
        self.scroll = self.scroll.clamp(0.0, max_scroll);
        if self.scroll >= max_scroll - 1.0 {
            self.stick_bottom = true;
        }

        ui.p.push_clip(area);
        let mut y = area.y - self.scroll;
        for (k, &(kind, i)) in msgs.iter().enumerate() {
            let h = heights[k];
            if y + h >= area.y && y <= area.bottom() {
                let t = if i < n_hist { self.history.get(i) } else { self.current.as_ref() };
                match kind {
                    Kind::User => {
                        ui.p.fill_rounded(
                            Rect::new(area.x + 4.0, y + 1.0, 2.4, (h - 2.0).max(4.0)),
                            1.2,
                            Color::white(0.32),
                        );
                        if let Some(t) = t {
                            ui.p.text(&t.user, Rect::new(col_x, y, col_w - 14.0, h), &ask);
                        }
                    }
                    Kind::Bot => {
                        if let Some(t) = t {
                            ui.p.text(&t.assistant, Rect::new(col_x, y, col_w, h), &answer);
                        }
                    }
                    Kind::Error => {
                        if let Some(e) = &self.error {
                            ui.p.text(e, Rect::new(col_x, y, col_w, h), &answer.color(palette::RED));
                        }
                    }
                    Kind::Working => ui.ticks_sweep(Rect::new(col_x, y + 2.0, 90.0, 8.0), Color::white(0.9)),
                }
            }
            y += h + gap_after(k);
        }
        ui.p.pop_clip();

        if max_scroll > 0.0 {
            let th = (area.h * area.h / total).max(18.0);
            let ty = area.y + (area.h - th) * (self.scroll / max_scroll);
            ui.p.fill_rounded(Rect::new(area.right() - 4.0, ty, 2.0, th), 1.0, Color::white(0.22));
        }
    }
}

/// Gemini's blue.
const GEM: Color = Color::hex(0x4285F4);

impl Gemini {
    /// Alcove: the key form as one soft card with a round badge.
    fn draw_key_form_alcove(&mut self, ui: &mut Ui, r: Rect) {
        let badge = Rect::new(r.x + 20.0, r.y + 20.0, 46.0, 46.0);
        crate::alcove::icon_chip(ui, badge, 2);
        ui.p.icon(Icon::Sparkle, badge.inset(12.0), GEM.lerp(Color::white(1.0), 0.3));
        ui.p.text(
            "Connect Gemini",
            Rect::new(badge.right() + 14.0, r.y + 18.0, r.w - 100.0, 28.0),
            &TextStyle::new(19.0, palette::TEXT).semibold(),
        );
        ui.p.text(
            "Paste your API key. It stays on this PC, in your DynamicNotch folder.",
            Rect::new(badge.right() + 14.0, r.y + 46.0, r.w - 100.0, 20.0),
            &TextStyle::new(12.5, palette::TEXT_DIM),
        );
        let row = Rect::new(r.x + 20.0, r.y + 84.0, r.w - 40.0, 42.0);
        let save = Rect::new(row.right() - 84.0, row.y, 84.0, row.h);
        let field = Rect::new(row.x, row.y, row.w - 94.0, row.h);
        ui.p.fill_rounded(field, field.h * 0.5, Color::white(0.09));
        if self.key_field.draw_bare(ui, field.inset_xy(18.0, 6.0), "AIza…", true, 15.0) {
            ui.fx.keyboard_focus = Some(true);
        }
        let can = !self.key_field.is_empty();
        if crate::alcove::chip(ui, save, "Save", None, can) && can && self.save_key() {
            ui.fx.layout_changed = true;
        }
        if let Some(e) = &self.key_error {
            ui.p.text(
                e,
                Rect::new(row.x + 8.0, row.bottom() + 6.0, row.w - 16.0, 18.0),
                &TextStyle::new(12.0, palette::RED),
            );
        }
        let lw = crate::alcove::chip_w(ui, "Get a free key", Some(Icon::Link));
        if crate::alcove::chip(
            ui,
            Rect::new(r.x + 20.0, r.bottom() - 46.0, lw, 30.0),
            "Get a free key",
            Some(Icon::Link),
            false,
        ) {
            shell_open("https://aistudio.google.com/apikey");
        }
        if self.has_key {
            let cw = crate::alcove::chip_w(ui, "Cancel", None);
            if crate::alcove::chip(
                ui,
                Rect::new(r.right() - 20.0 - cw, r.bottom() - 46.0, cw, 30.0),
                "Cancel",
                None,
                false,
            ) {
                self.editing_key = false;
                self.key_field.clear();
                ui.fx.layout_changed = true;
            }
        }
    }

    /// Alcove: bubbles above, a capsule to type in below.
    fn draw_chat_alcove(&mut self, ui: &mut Ui, r: Rect) {
        let input = Rect::new(r.x, r.bottom() - 48.0, r.w, 48.0);
        let log = Rect::new(r.x, r.y, r.w, (input.y - 12.0 - r.y).max(0.0));
        self.draw_log_alcove(ui, log);

        ui.p.icon(
            Icon::Sparkle,
            Rect::centered(input.x + 26.0, input.cy(), 16.0, 16.0),
            GEM.lerp(Color::white(1.0), 0.3),
        );

        let can = !self.input.is_empty();
        let mut x = input.right() - 42.0;
        let send = Rect::new(x, input.cy() - 18.0, 36.0, 36.0);
        if self.streaming {
            if crate::alcove::round_button(ui, send, Icon::Stop, true) {
                self.stop_stream();
            }
        } else if crate::alcove::round_button(ui, send, Icon::Send, can) && can {
            self.submit();
        }
        let small = |x: f32| Rect::new(x, input.cy() - 15.0, 30.0, 30.0);
        if self.response().is_some() {
            x -= 36.0;
            if crate::alcove::round_button(ui, small(x), Icon::Refresh, false) {
                self.new_chat();
                ui.fx.layout_changed = true;
            }
            if let Some(t) = self.response() {
                if !t.assistant.is_empty() && !self.streaming {
                    x -= 34.0;
                    let text = t.assistant.clone();
                    if crate::alcove::round_button(ui, small(x), Icon::Copy, false) {
                        clip::set_text(self.hwnd.hwnd(), &text);
                    }
                }
            }
        }
        x -= 34.0;
        if crate::alcove::round_button(ui, small(x), Icon::Lock, false) {
            self.editing_key = true;
            self.key_field.clear();
            ui.fx.layout_changed = true;
        }
        let hint = if self.streaming { "Answering…" } else { "Ask Gemini anything" };
        let fx = input.x + 52.0;
        let field = Rect::new(fx, input.y + 8.0, (x - 8.0 - fx).max(80.0), 32.0);
        if self.input.draw_bare(ui, field, hint, true, 15.0) {
            ui.fx.keyboard_focus = Some(true);
        }
    }

    fn draw_log_alcove(&mut self, ui: &mut Ui, area: Rect) {
        #[derive(PartialEq, Clone, Copy)]
        enum Kind {
            User,
            Bot,
            Error,
            Working,
        }
        let ask_line = TextStyle::new(14.5, palette::INK);
        let ask = TextStyle::new(14.5, palette::INK).wrap().top();
        let answer = TextStyle::new(14.5, Color::white(0.94)).wrap().top();
        let (px, py) = (16.0, 11.0);
        let max_user = (area.w * 0.72).min(area.w - 60.0).max(120.0);
        let bot_x = 40.0;
        let max_bot = (area.w - bot_x - 6.0).max(160.0);
        let gap = 10.0;

        let n_hist = self.history.len();
        let mut msgs: Vec<(Kind, usize)> = Vec::new();
        for i in 0..n_hist + usize::from(self.current.is_some()) {
            let t = if i < n_hist { &self.history[i] } else { self.current.as_ref().unwrap() };
            msgs.push((Kind::User, i));
            if !t.assistant.is_empty() {
                msgs.push((Kind::Bot, i));
            }
            if self.streaming && i >= n_hist && t.assistant.is_empty() {
                msgs.push((Kind::Working, i));
            }
        }
        if self.error.is_some() {
            msgs.push((Kind::Error, 0));
        }

        if msgs.is_empty() {
            self.text_h = 0.0;
            let cx = area.cx();
            crate::alcove::icon_chip(ui, Rect::centered(cx, area.cy() - 34.0, 38.0, 38.0), 3);
            ui.p.icon(
                Icon::Sparkle,
                Rect::centered(cx, area.cy() - 34.0, 22.0, 22.0),
                GEM.lerp(Color::white(1.0), 0.3),
            );
            ui.p.text(
                "How can I help?",
                Rect::new(area.x, area.cy() - 8.0, area.w, 30.0),
                &TextStyle::new(20.0, Color::white(0.95)).semibold().center(),
            );
            return;
        }

        // (height, width) of every bubble
        let mut sizes: Vec<(f32, f32)> = Vec::with_capacity(msgs.len());
        for &(kind, i) in &msgs {
            let t = if i < n_hist { self.history.get(i) } else { self.current.as_ref() };
            let size = match kind {
                Kind::User => {
                    let text = t.map(|t| t.user.clone()).unwrap_or_default();
                    let (lw, _) = ui.p.measure(&text, &ask_line);
                    let w = (lw + 2.0 * px + 4.0).min(max_user);
                    (ui.p.measure_wrapped(&text, &ask, w - 2.0 * px) + 2.0 * py, w)
                }
                Kind::Bot => {
                    let text = t.map(|t| t.assistant.clone()).unwrap_or_default();
                    (ui.p.measure_wrapped(&text, &answer, max_bot - 2.0 * px) + 2.0 * py, max_bot)
                }
                Kind::Error => {
                    let text = self.error.clone().unwrap_or_default();
                    (ui.p.measure_wrapped(&text, &answer, max_bot - 2.0 * px) + 2.0 * py, max_bot)
                }
                Kind::Working => (36.0, 70.0),
            };
            sizes.push(size);
        }
        let total: f32 = sizes.iter().map(|s| s.0 + gap).sum::<f32>() - gap;
        self.text_h = total + 12.0;

        let max_scroll = (total - area.h).max(0.0);
        let wheel = ui.wheel(area);
        if wheel != 0.0 {
            self.scroll -= wheel * 40.0;
            self.stick_bottom = self.scroll >= max_scroll - 1.0;
        }
        if self.stick_bottom {
            self.scroll = max_scroll;
        }
        self.scroll = self.scroll.clamp(0.0, max_scroll);
        if self.scroll >= max_scroll - 1.0 {
            self.stick_bottom = true;
        }

        ui.p.push_clip(area);
        let mut y = area.y - self.scroll;
        for (k, &(kind, i)) in msgs.iter().enumerate() {
            let (h, w) = sizes[k];
            if y + h >= area.y && y <= area.bottom() {
                let t = if i < n_hist { self.history.get(i) } else { self.current.as_ref() };
                let rad = (h * 0.5).min(21.0);
                match kind {
                    Kind::User => {
                        let b = Rect::new(area.right() - w - 6.0, y, w, h);
                        ui.p.fill_rounded(b, rad, Color::white(0.94));
                        if let Some(t) = t {
                            ui.p.text(&t.user, Rect::new(b.x + px, b.y + py, b.w - 2.0 * px, b.h - 2.0 * py), &ask);
                        }
                    }
                    Kind::Bot | Kind::Error | Kind::Working => {
                        ui.p.icon(
                            Icon::Sparkle,
                            Rect::centered(area.x + 16.0, y + 16.0, 15.0, 15.0),
                            GEM.lerp(Color::white(1.0), 0.3),
                        );
                        let b = Rect::new(area.x + bot_x, y, w, h);
                        ui.p.fill_rounded(b, rad, Color::white(0.08));
                        let inner = Rect::new(b.x + px, b.y + py, b.w - 2.0 * px, b.h - 2.0 * py);
                        match kind {
                            Kind::Bot => {
                                if let Some(t) = t {
                                    ui.p.text(&t.assistant, inner, &answer);
                                }
                            }
                            Kind::Error => {
                                if let Some(e) = &self.error {
                                    ui.p.text(e, inner, &answer.color(palette::RED));
                                }
                            }
                            _ => {
                                let now = crate::ui::epoch_secs();
                                for d in 0..3 {
                                    let ph = (now * 5.0 - d as f32 * 0.8).sin() * 0.5 + 0.5;
                                    ui.p.fill_circle(
                                        b.x + 22.0 + d as f32 * 14.0,
                                        b.cy() - 3.0 * ph,
                                        3.4 + ph,
                                        Color::white(0.45 + 0.5 * ph),
                                    );
                                }
                                ui.animate();
                            }
                        }
                    }
                }
            }
            y += h + gap;
        }
        ui.p.pop_clip();
    }
}
