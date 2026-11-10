//! Greets you when you unlock the PC: a Face ID style scan, then "Hello"
//! (or "Hello Sam") written out in blue handwriting.
//!
//! The shell registers for session notifications and forwards an `unlocked`
//! command; this module waits a moment (the desktop is still coming back) and
//! then shows the peek. Switch it off in Settings or with `greeting = false`
//! under `[modules]`; set `[greeting] name` (Settings → General) to be greeted by name.

use std::time::{Duration, Instant};

use super::{Cx, Module, ModuleId, Peek, SystemEvent};
use crate::gfx::Icon;
use crate::util::Color;

const ID: ModuleId = "greeting";
/// Let the desktop finish appearing before the peek slides out.
const DELAY: Duration = Duration::from_millis(900);
/// Unlock events can repeat (remote sessions, fast user switching).
const MIN_GAP: Duration = Duration::from_secs(10);

pub struct Greeting {
    due: Option<Instant>,
    last: Option<Instant>,
}

impl Greeting {
    pub fn new() -> Self {
        Self { due: None, last: None }
    }
}

impl Module for Greeting {
    fn id(&self) -> ModuleId {
        ID
    }
    fn title(&self) -> &str {
        "Unlock greeting"
    }
    fn icon(&self) -> Icon {
        Icon::Sparkle
    }

    fn stop(&mut self) {
        self.due = None;
    }

    fn on_system(&mut self, ev: &SystemEvent, cx: &mut Cx) {
        if let SystemEvent::Command { verb, .. } = ev {
            if verb == "unlocked" && self.last.is_none_or(|t| t.elapsed() > MIN_GAP) {
                self.due = Some(cx.now + DELAY);
            }
        }
    }

    fn next_tick(&self) -> Option<Instant> {
        self.due
    }

    fn on_tick(&mut self, cx: &mut Cx) {
        if !self.due.is_some_and(|d| cx.now >= d) {
            return;
        }
        self.due = None;
        self.last = Some(cx.now);
        // Face ID style scan in blue that ends in a check, then the notch writes "Hello"
        // (or "Hello <your name>", from Settings → General) in handwriting.
        let name: String = cx.cfg.greeting.name.trim().chars().take(24).collect();
        let title = if name.is_empty() { "Hello".to_string() } else { format!("Hello {name}") };
        let peek = Peek::new(Icon::Unlock, Color::hex(0x007AFF), title, "").scan().duration_ms(if crate::ui::look_alcove() { 2100 } else { 5200 });
        cx.fx.peek(peek.key("greeting"));
        cx.fx.redraw = true;
    }
}
