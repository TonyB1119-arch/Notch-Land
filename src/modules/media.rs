//! Now playing — Windows Global System Media Transport Controls (the same
//! source the Windows volume flyout uses). Works with Spotify, browsers,
//! Media Player, VLC, ... anything that integrates with SMTC.
//!
//! A worker thread owns the WinRT session manager, subscribes to its
//! events and pushes immutable snapshots to the UI thread.

use std::any::Any;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Foundation::TypedEventHandler;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session, GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
};
use windows::Storage::Streams::DataReader;
use windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows_future::IAsyncOperation;

use super::{Activity, CardSize, Cx, Module, ModuleId, Peek, Slot, SystemEvent};
use crate::anim::smoothstep;
use crate::bus::Bus;
use crate::gfx::{image, Icon, ImageData, TextStyle};
use crate::sys::http::{self, Timeouts};
use crate::ui::{ButtonStyle, Ui};
use crate::util::{format_duration, palette, Color, Rect};

const ID: ModuleId = "media";

/// The media page: a big cover on the left, the title, artist and the three buttons on the right.
const PAGE_H: f32 = 124.0;
const PAGE_ART: f32 = 112.0;

#[derive(Clone, Debug, Default)]
struct Snapshot {
    active: bool,
    app: String,
    title: String,
    artist: String,
    playing: bool,
    /// Position at `stamp`, seconds.
    position: f64,
    duration: f64,
    can_seek: bool,
    can_next: bool,
    can_prev: bool,
    art: Option<ImageData>,
}

enum Cmd {
    Refresh,
    PlayPause,
    Next,
    Prev,
    Seek(f64),
    Quit,
}

pub struct Media {
    snap: Snapshot,
    stamp: Instant,
    tx: Option<Sender<Cmd>>,
    paused_at: Option<Instant>,
    /// A different track (or none) that has not lasted long enough yet, and since when: closing a
    /// tab makes browsers flash their last few sessions for a moment, and those must not show.
    pending: Option<(Instant, Snapshot)>,
    /// Set while a settled snapshot is replayed from `on_tick`.
    settled: bool,
    /// When the player last started playing (a browser must play steadily before it counts).
    play_since: Option<Instant>,
    expanded: bool,
    linger: Duration,
    first: bool,
    /// Optimistic UI state after a click, until the worker confirms.
    pending_playing: Option<(bool, Instant)>,
    /// Rotation of the vinyl shown when there is no artwork (radians).
    spin: f32,
    spin_at: Instant,
    /// When the player card was last drawn: ticks only run while it is on screen.
    last_draw: Option<Instant>,
    /// The first track change of a listening session gets a peek; after that
    /// the visible peek and the player card just animate to the next track.
    peeked: bool,
    /// The track that was playing before the last change, and when it changed.
    prev: Option<(String, String, Option<ImageData>)>,
    changed_at: Option<Instant>,
    /// Synced lyrics of the current track (seconds, text), once found.
    lyrics: Vec<(f64, String)>,
    /// The track the lyrics were asked for, so a late answer for an old track is dropped.
    lyrics_key: String,
    /// Lyrics or controls on the media page (click the cover to switch).
    show_lyrics: bool,
    /// The line shown now and when it changed (for the scroll).
    ly_idx: usize,
    ly_at: Instant,
    /// After a click on the output button: what it should show until the system confirms.
    out_want: Option<(bool, Instant)>,
    /// The app that plays (Spotify...) and its icon for the corner of the cover, once found.
    badge_app: String,
    badge: Option<ImageData>,
}

/// The icon of the app that plays, found in the background (`app` says which one it is for).
struct Badge {
    app: String,
    img: Option<ImageData>,
}

/// Look for the playing app among the open windows and take its icon.
fn spawn_badge(app: String, bus: Bus) {
    if app.is_empty() {
        return;
    }
    let _ = std::thread::Builder::new().name("media-badge".into()).spawn(move || {
        let hint = match app.as_str() {
            "Microsoft Edge" => "msedge".to_string(),
            other => other.to_lowercase().replace(' ', ""),
        };
        let found = crate::switcher::list().into_iter().find(|w| {
            let name = w.app().to_lowercase();
            !name.is_empty() && (name == hint || name.contains(&hint) || hint.contains(&name))
        });
        let img = found.and_then(|w| crate::dock::icon_for_path(&w.exe, &w.app()));
        bus.to_module(ID, Badge { app, img });
    });
}

/// A lyrics answer from the worker thread.
struct Lyrics {
    key: String,
    lines: Vec<(f64, String)>,
}

/// How long the cover flip takes when the track changes (seconds).
/// How long a browser must have been playing before the notch believes it.
const PLAY_GATE: Duration = Duration::from_millis(800);

fn is_browser(app: &str) -> bool {
    matches!(app, "Chrome" | "Microsoft Edge" | "Firefox" | "Brave" | "Opera" | "Vivaldi")
}

/// How long a paused browser tab stays on the notch.
const BROWSER_LINGER: Duration = Duration::from_secs(3);

/// How long a different track (or the player going away) must last before the notch follows.
const SETTLE: Duration = Duration::from_millis(700);

const FLIP_SECS: f32 = 0.6;

fn track_key(title: &str, artist: &str) -> String {
    format!("{title}\u{1}{artist}")
}

/// How long the switch to the next track animates (seconds).
const SWITCH_SECS: f32 = 0.7;

impl Media {
    pub fn new() -> Self {
        Self {
            snap: Snapshot::default(),
            stamp: Instant::now(),
            tx: None,
            paused_at: None,
            pending: None,
            settled: false,
            play_since: None,
            expanded: false,
            linger: Duration::from_secs(20),
            first: true,
            pending_playing: None,
            spin: 0.5,
            spin_at: Instant::now(),
            last_draw: None,
            peeked: false,
            prev: None,
            changed_at: None,
            lyrics: Vec::new(),
            lyrics_key: String::new(),
            show_lyrics: true,
            ly_idx: 0,
            ly_at: Instant::now(),
            out_want: None,
            badge_app: String::new(),
            badge: None,
        }
    }

    fn send(&self, c: Cmd) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(c);
        }
    }

    fn playing(&self) -> bool {
        match self.pending_playing {
            Some((p, t)) if t.elapsed() < Duration::from_millis(1500) => p,
            _ => self.snap.playing && self.stable_play(),
        }
    }

    /// Browsers flash half-second "plays" from tabs that are going away; only steady playing counts.
    fn stable_play(&self) -> bool {
        !is_browser(&self.snap.app) || self.play_since.is_none_or(|t| t.elapsed() >= PLAY_GATE)
    }

    fn position(&self) -> f64 {
        let mut p = self.snap.position;
        if self.snap.playing {
            p += self.stamp.elapsed().as_secs_f64();
        }
        if self.snap.duration > 0.0 {
            p = p.min(self.snap.duration);
        }
        p.max(0.0)
    }

    /// Progress (0..1) of the cover flip after a track change, while it runs.
    fn flip(&self) -> Option<f32> {
        let t = self.changed_at?.elapsed().as_secs_f32() / FLIP_SECS;
        (t < 1.0 && crate::ui::look_alcove()).then_some(t)
    }

    /// How long a paused player stays on the notch. A browser tab that paused (leaving YouTube
    /// Shorts, say) is not worth keeping for long.
    fn linger_now(&self) -> Duration {
        if is_browser(&self.snap.app) {
            self.linger.min(BROWSER_LINGER)
        } else {
            self.linger
        }
    }

    fn visible(&self) -> bool {
        self.snap.active && (self.playing() || self.paused_at.is_some_and(|t| t.elapsed() < self.linger_now()))
    }

    fn accent(&self) -> Color {
        self.snap.art.as_ref().and_then(|a| a.accent).unwrap_or(Color::white(0.9))
    }

    /// Headphones (true) or the built-in speakers (false), as far as the system says; right after a
    /// click the answer it is going to give.
    fn out_headphones(&self) -> bool {
        match self.out_want {
            Some((hp, t)) if t.elapsed() < Duration::from_secs(3) => hp,
            _ => super::osd::output_is_headphones(),
        }
    }

    /// The button that moves the sound to the next output device: a speaker for the built-in
    /// ones, headphones for AirPods and the like, one turning into the other with a short fade.
    fn output_button(&mut self, ui: &mut Ui, r: Rect, fg: Color, scale: f32) {
        let hp = self.out_headphones();
        let (h, p) = ui.touch(r, "output-button");
        ui.p.fill_rounded(r.inset(r.w * 0.07 * p), r.h * 0.5, Color::white(0.08 * h));
        let k = ui.glide(0x0A07_0001, if hp { 1.0 } else { 0.0 });
        let size = r.w.min(r.h) * scale * (1.0 - 0.14 * p) * (1.0 + 0.03 * h);
        let (cx, cy) = (r.cx(), r.cy());
        let old = ui.p.alpha;
        // the speaker leaves downward as the headphones come up from below
        for (icon, a, dy) in [(Icon::Volume, 1.0 - k, k * 5.0), (Icon::Headphones, k, (1.0 - k) * 5.0)] {
            if a > 0.005 {
                ui.p.alpha = old * a;
                let s = size * (0.7 + 0.3 * a);
                ui.p.icon(icon, Rect::new(cx - s * 0.5, cy + dy - s * 0.5, s, s), fg);
            }
        }
        ui.p.alpha = old;
        if ui.clicked(r) {
            self.out_want = Some((!hp, Instant::now()));
            super::osd::request_next_output(ui.bus);
        }
    }

    fn toggle(&mut self) {
        let p = !self.playing();
        self.pending_playing = Some((p, Instant::now()));
        self.send(Cmd::PlayPause);
    }
}

impl Media {
    fn draw_titles(&self, ui: &mut Ui, col: Rect, title: &str, sub: &str, dy: f32) {
        ui.p.text(title, Rect::new(col.x, col.y + dy, col.w, 20.0), &TextStyle::new(15.5, palette::TEXT).semibold());
        ui.p.text(sub, Rect::new(col.x, col.y + 19.0 + dy, col.w, 18.0), &TextStyle::new(12.5, palette::TEXT_DIM));
    }
}

impl Module for Media {
    fn id(&self) -> ModuleId {
        ID
    }
    fn title(&self) -> &str {
        "Media"
    }
    fn icon(&self) -> Icon {
        Icon::Music
    }

    fn start(&mut self, cx: &mut Cx) {
        self.linger = Duration::from_secs(cx.cfg.media.linger_after_pause_secs as u64);
        let (tx, rx) = channel();
        let bus = cx.bus.clone();
        let tx2 = tx.clone();
        let _ = std::thread::Builder::new().name("media".into()).spawn(move || worker(rx, tx2, bus));
        self.tx = Some(tx);
    }

    fn stop(&mut self) {
        self.send(Cmd::Quit);
        self.tx = None;
        self.snap = Snapshot::default();
    }

    fn on_message(&mut self, msg: Box<dyn Any + Send>, cx: &mut Cx) {
        let msg = match msg.downcast::<Lyrics>() {
            Ok(l) => {
                if l.key == self.lyrics_key {
                    self.lyrics = l.lines;
                    cx.fx.redraw = true;
                }
                return;
            }
            Err(m) => m,
        };
        let msg = match msg.downcast::<Badge>() {
            Ok(b) => {
                if b.app == self.badge_app {
                    self.badge = b.img;
                    cx.fx.redraw = true;
                }
                return;
            }
            Err(m) => m,
        };
        let Ok(s) = msg.downcast::<Snapshot>() else { return };
        let s = *s;
        // a new track (or the player going away) only counts once it has lasted a moment; play and
        // pause and the progress apply at once
        let settled = std::mem::take(&mut self.settled);
        let same = |a: &Snapshot, b: &Snapshot| a.active == b.active && a.title == b.title && a.artist == b.artist;
        if !settled && !self.first && !same(&s, &self.snap) {
            let since = match &self.pending {
                Some((t, p)) if same(p, &s) => *t,
                _ => Instant::now(),
            };
            if since.elapsed() < SETTLE {
                self.pending = Some((since, s));
                cx.fx.redraw = true;
                return;
            }
        }
        self.pending = None;
        let key = track_key(&s.title, &s.artist);
        if key != self.lyrics_key {
            self.lyrics.clear();
            self.ly_idx = 0;
            self.lyrics_key = key.clone();
            if !s.title.is_empty() {
                let (bus, title, artist, dur) = (cx.bus.clone(), s.title.clone(), s.artist.clone(), s.duration);
                let _ = std::thread::Builder::new().name("lyrics".into()).spawn(move || {
                    if let Some(lines) = fetch_lyrics(&title, &artist, dur) {
                        bus.to_module(ID, Lyrics { key, lines });
                    }
                });
            }
        }
        let track_changed = s.title != self.snap.title || s.artist != self.snap.artist;
        if track_changed && !self.snap.title.is_empty() && !s.title.is_empty() {
            self.prev = Some((self.snap.title.clone(), self.snap.artist.clone(), self.snap.art.clone()));
            self.changed_at = Some(Instant::now());
        }
        if !s.active {
            self.peeked = false; // the next time something plays, it gets its peek again
        }
        if self.snap.playing && !s.playing && self.stable_play() {
            self.paused_at = Some(Instant::now());
        }
        if s.playing && !self.snap.playing {
            self.play_since = Some(Instant::now());
        }
        if !s.playing {
            self.play_since = None;
        }
        if s.playing && !is_browser(&s.app) {
            self.paused_at = None;
        }
        if !self.first
            && track_changed
            && s.active
            && s.playing
            && !s.title.is_empty()
            && cx.cfg.media.peek_on_track_change
            && !self.expanded
        {
            let p = Peek::new(
                Icon::Music,
                s.art.as_ref().and_then(|a| a.accent).unwrap_or(palette::PINK),
                &s.title,
                &s.artist,
            )
            .image(s.art.clone())
            .key("media")
            .duration_ms(3000);
            if self.peeked {
                // later tracks: the open peek just animates to the new one
                cx.fx.peek(p.quiet());
            } else {
                self.peeked = true;
                cx.fx.peek(p);
            }
        }
        self.first = false;
        // Keep the optimistic play/pause icon until the player confirms the
        // change; snapshots read right after the click are often stale and
        // would make the icon flicker (pause → play → pause → play).
        if let Some((want, at)) = self.pending_playing {
            if s.playing == want || at.elapsed() > Duration::from_millis(1500) {
                self.pending_playing = None;
            }
        }
        self.snap = s;
        self.stamp = Instant::now();
        cx.fx.redraw = true;
    }

    fn on_system(&mut self, ev: &SystemEvent, cx: &mut Cx) {
        match ev {
            SystemEvent::Expanded(e) => self.expanded = *e,
            SystemEvent::ConfigReloaded => {
                self.linger = Duration::from_secs(cx.cfg.media.linger_after_pause_secs as u64)
            }
            SystemEvent::Command { verb, .. } => match verb.as_str() {
                "playpause" | "play" | "pause" => self.toggle(),
                "next" => self.send(Cmd::Next),
                "prev" | "previous" => self.send(Cmd::Prev),
                _ => {}
            },
            _ => {}
        }
    }

    fn next_tick(&self) -> Option<Instant> {
        // the record and the progress bar only need frames while the card is on screen
        let seen = self.last_draw.is_some_and(|t| t.elapsed() < Duration::from_millis(900));
        if self.expanded && self.snap.active && seen {
            if self.playing() {
                return Some(Instant::now() + Duration::from_millis(40));
            }
            if self.snap.playing {
                return Some(Instant::now() + Duration::from_millis(500));
            }
        }
        // when the paused linger expires the activity disappears; a waiting change settles then too
        let now = Instant::now();
        let linger = self.paused_at.map(|t| t + self.linger_now()).filter(|t| *t > now);
        let settle = self.pending.as_ref().map(|(t, _)| *t + SETTLE).filter(|t| *t > now);
        let gate = self
            .play_since
            .filter(|_| self.snap.playing && is_browser(&self.snap.app))
            .map(|t| t + PLAY_GATE)
            .filter(|t| *t > now);
        [linger, settle, gate].into_iter().flatten().min()
    }

    fn on_tick(&mut self, cx: &mut Cx) {
        if self.snap.playing && self.stable_play() {
            self.paused_at = None;
        }
        if self.pending.as_ref().is_some_and(|(t, _)| t.elapsed() + Duration::from_millis(5) >= SETTLE) {
            if let Some((_, snap)) = self.pending.take() {
                self.settled = true;
                self.on_message(Box::new(snap), cx);
            }
        }
        cx.fx.redraw = true;
    }

    fn is_playing(&self) -> bool {
        self.playing()
    }

    fn activity(&self) -> Option<Activity> {
        if !self.visible() {
            return None;
        }
        let flip = self.flip().map(|p| (p, self.prev.as_ref().and_then(|x| x.2.clone())));
        let left = Slot::Vinyl { color: self.accent(), angle: self.spin, art: self.snap.art.clone(), flip };
        Some(Activity {
            priority: if self.playing() { 50 } else { 10 },
            left,
            right: Slot::Bars { color: self.accent(), playing: self.playing() },
            wing: 1.05,
            page: if crate::ui::look_alcove() { Some(ID) } else { None },
        })
    }

    fn has_page(&self) -> bool {
        crate::ui::look_alcove() && self.snap.active
    }

    fn page_height(&self, _width: f32) -> f32 {
        PAGE_H
    }

    fn draw_page(&mut self, ui: &mut Ui, r: Rect) {
        self.last_draw = Some(ui.now);
        let playing = self.playing();
        let lyrics_on = self.show_lyrics && !self.lyrics.is_empty();
        // which app plays: its icon sits in the corner of the cover
        if self.badge_app != self.snap.app {
            self.badge_app = self.snap.app.clone();
            self.badge = None;
            spawn_badge(self.snap.app.clone(), ui.bus.clone());
        }

        // the cover, big and round (it flips over when the track changes); a click switches lyrics and text
        let art_s = r.h.min(PAGE_ART);
        let art = Rect::new(r.x, r.cy() - art_s * 0.5, art_s, art_s);
        if !self.lyrics.is_empty() && ui.clicked(art) {
            self.show_lyrics = !self.show_lyrics;
        }
        let (mut shown, mut k) = (self.snap.art.as_ref(), 1.0f32);
        let flipping = self.flip();
        if let Some(p) = flipping {
            k = (p * std::f32::consts::PI).cos().abs().max(0.02);
            if p < 0.5 {
                shown = self.prev.as_ref().and_then(|x| x.2.as_ref());
            }
            ui.animate();
        }
        let face = Rect::centered(art.cx(), art.cy(), art_s * k, art_s * (0.93 + 0.07 * k));
        match shown {
            Some(img) => ui.p.image(img, face, art_s * 0.24 * k.max(0.4), 1.0),
            None => {
                ui.p.fill_rounded(face, art_s * 0.24 * k.max(0.4), Color::white(0.08));
                ui.p.icon(
                    Icon::Music,
                    Rect::centered(face.cx(), face.cy(), face.w * 0.4, art_s * 0.4),
                    Color::white(0.55),
                );
            }
        }
        if let (Some(b), None) = (&self.badge, flipping) {
            let bs = art_s * 0.27;
            let back = Rect::new(art.x + art_s * 0.07, art.bottom() - art_s * 0.07 - bs - 8.0, bs + 8.0, bs + 8.0);
            ui.p.fill_rounded(back, 11.0, Color::black(0.88));
            ui.p.image(b, back.inset(4.0), 8.0, 1.0);
        }

        // the words and the buttons, together in the middle of the right side
        let tx = art.right() + 18.0;
        let tw = (r.right() - tx).max(60.0);
        let title = if self.snap.title.is_empty() { "Unknown title" } else { &self.snap.title };
        let sub = if !self.snap.artist.is_empty() { self.snap.artist.clone() } else { self.snap.app.clone() };
        let tst = TextStyle::new(18.0, Color::white(1.0)).semibold().top().wrap();
        let (_, line) = ui.p.measure(title, &tst);
        let lines = (ui.p.measure_wrapped(title, &tst, tw) / line.max(1.0)).round().clamp(1.0, 2.0);
        let th = line * lines;
        let text_h = if lyrics_on { 58.0 } else { th + 2.0 + 22.0 };
        let ctl_h = 40.0;
        let y0 = r.cy() - (text_h + 8.0 + ctl_h) * 0.5;
        if lyrics_on {
            let pos = self.position();
            self.draw_lyrics(ui, Rect::new(tx, y0, tw, text_h), pos);
            if playing {
                // the line moves on by itself
                ui.animate_slow();
            }
        } else {
            let box_ = Rect::new(tx, y0, tw, th);
            ui.p.push_clip(box_);
            ui.p.text(title, box_, &tst);
            ui.p.pop_clip();
            ui.p.text(&sub, Rect::new(tx, y0 + th + 2.0, tw, 22.0), &TextStyle::new(15.0, palette::TEXT_DIM));
        }

        // back, play or pause, forward: plain white glyphs, left-aligned under the words
        let cy = y0 + text_h + 8.0 + ctl_h * 0.5;
        let dim = |on: bool| if on { Color::white(0.97) } else { Color::white(0.35) };
        let side = ButtonStyle::default().scale(0.85);
        let x1 = tx + 14.0;
        if ui.icon_button(Rect::centered(x1, cy, 44.0, ctl_h), Icon::Prev, side.fg(dim(self.snap.can_prev))) {
            self.send(Cmd::Prev);
        }
        let pp = if playing { Icon::Pause } else { Icon::Play };
        if ui.icon_button(
            Rect::centered(x1 + 56.0, cy, 52.0, 44.0),
            pp,
            ButtonStyle::default().fg(Color::white(1.0)).scale(0.95),
        ) {
            self.toggle();
        }
        if ui.icon_button(Rect::centered(x1 + 112.0, cy, 44.0, ctl_h), Icon::Next, side.fg(dim(self.snap.can_next))) {
            self.send(Cmd::Next);
        }
        self.output_button(ui, Rect::centered(x1 + 168.0, cy, 44.0, ctl_h), Color::white(0.97), 0.85);
    }

    fn card(&self) -> Option<CardSize> {
        if self.snap.active {
            Some(CardSize::Large)
        } else {
            None
        }
    }

    fn draw_card(&mut self, ui: &mut Ui, r: Rect) {
        ui.card(r, false);
        let pad = 12.0;
        let art_s = (r.h - 2.0 * pad).min(r.w * 0.42);
        let art = Rect::new(r.x + pad, r.y + pad, art_s, art_s);
        // a black record that turns while the music plays, the cover as its label
        let dt = ui.now.saturating_duration_since(self.spin_at).as_secs_f32().min(0.1);
        self.spin_at = ui.now;
        self.last_draw = Some(ui.now);
        if self.playing() {
            // one slow, calm turn every 16 seconds
            self.spin = (self.spin + dt * std::f32::consts::TAU / 16.0) % std::f32::consts::TAU;
        }
        // switching tracks: the record dips and the label changes at the bottom of the dip,
        // the old title drifts up and fades while the new one rises in
        let t = self
            .changed_at
            .map(|at| ui.now.saturating_duration_since(at).as_secs_f32() / SWITCH_SECS)
            .filter(|t| *t < 1.0);
        let (dip, old_a, new_a, old_dy, new_dy) = match t {
            Some(t) => {
                ui.animate();
                let out = smoothstep(0.0, 0.5, t);
                let inn = smoothstep(0.4, 1.0, t);
                ((t * std::f32::consts::PI).sin(), 1.0 - out, inn, -10.0 * out, 10.0 * (1.0 - inn))
            }
            None => (0.0, 0.0, 1.0, 0.0, 0.0),
        };
        let showing_old = t.is_some_and(|t| t < 0.5);
        let art_img = match (&self.prev, showing_old) {
            (Some((_, _, a)), true) => a.as_ref(),
            _ => self.snap.art.as_ref(),
        };
        ui.vinyl(art.inset(art.w * 0.07 * dip), self.spin + dip * 0.9, self.accent(), art_img);
        let col = Rect::new(art.right() + 14.0, r.y + pad, r.right() - art.right() - 14.0 - pad, r.h - 2.0 * pad);
        let sub_of = |title: &str, artist: &str, app: &str| {
            if !artist.is_empty() {
                artist.to_string()
            } else if app != title {
                app.to_string()
            } else {
                String::new()
            }
        };
        let cur_title = if self.snap.title.is_empty() { "Unknown title" } else { &self.snap.title };
        let cur_sub = sub_of(cur_title, &self.snap.artist, &self.snap.app);
        let head = Rect::new(col.x, col.y - 2.0, col.w, 42.0);
        let a0 = ui.p.alpha;
        ui.p.push_clip(head);
        if let (Some((pt, pa, _)), true) = (&self.prev, t.is_some()) {
            ui.p.alpha = a0 * old_a;
            let psub = sub_of(pt, pa, &self.snap.app);
            self.draw_titles(ui, col, pt, &psub, old_dy);
        }
        ui.p.alpha = a0 * new_a;
        self.draw_titles(ui, col, cur_title, &cur_sub, new_dy);
        ui.p.alpha = a0;
        ui.p.pop_clip();

        // progress
        let bar_y = col.y + 41.0;
        let pos = self.position();
        let dur = self.snap.duration;
        let bar = Rect::new(col.x + 4.0, bar_y, col.w - 8.0, 6.0);
        let frac = if dur > 0.0 { (pos / dur) as f32 } else { 0.0 };
        let hit = bar.inset_xy(-4.0, -9.0);
        let hover = self.snap.can_seek && dur > 0.0 && ui.hovered(hit);
        ui.scrubber(bar, frac, hover);
        if self.snap.can_seek && dur > 0.0 {
            if let Some(f) = ui.clicked_at(hit) {
                let target = f as f64 * dur;
                self.snap.position = target;
                self.stamp = Instant::now();
                self.send(Cmd::Seek(target));
            }
        }
        // transport row: times on the sides, controls in the middle (below the scrubber)
        let cy = col.bottom() - 17.0;
        if dur > 0.0 {
            let ts = TextStyle::new(10.5, palette::TEXT_FAINT).display();
            ui.p.text(&format_duration(pos), Rect::new(col.x, cy - 7.0, 60.0, 14.0), &ts);
            ui.p.text(
                &format!("-{}", format_duration(dur - pos)),
                Rect::new(col.right() - 60.0, cy - 7.0, 60.0, 14.0),
                &ts.right(),
            );
        }

        // transport controls
        let big = 34.0;
        let small = 28.0;
        let cx = col.cx();
        let style = ButtonStyle::default().scale(0.5);
        let prev = Rect::centered(cx - big * 0.5 - 14.0 - small * 0.5, cy, small, small);
        let play = Rect::centered(cx, cy, big, big);
        let next = Rect::centered(cx + big * 0.5 + 14.0 + small * 0.5, cy, small, small);
        let dim = |on: bool| if on { palette::TEXT } else { palette::TEXT_FAINT };
        if ui.icon_button(prev, Icon::Prev, style.fg(dim(self.snap.can_prev))) {
            self.send(Cmd::Prev);
        }
        let pp = if self.playing() { Icon::Pause } else { Icon::Play };
        if ui.icon_button(play, pp, ButtonStyle::inverted().scale(0.5)) {
            self.toggle();
        }
        if ui.icon_button(next, Icon::Next, style.fg(dim(self.snap.can_next))) {
            self.send(Cmd::Next);
        }
        let out = Rect::centered(next.cx() + small + 14.0, cy, small, small);
        self.output_button(ui, out, palette::TEXT, 0.5);
    }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

impl Media {
    /// Two lines of the song: the one being sung and the next, scrolling up as it moves on.
    fn draw_lyrics(&mut self, ui: &mut Ui, area: Rect, pos: f64) {
        let idx = self.lyrics.partition_point(|(t, _)| *t <= pos + 0.25).saturating_sub(1);
        if idx != self.ly_idx {
            self.ly_idx = idx;
            self.ly_at = ui.now;
        }
        let t = (ui.now.saturating_duration_since(self.ly_at).as_secs_f32() / 0.38).min(1.0);
        let e = smoothstep(0.0, 1.0, t);
        if t < 1.0 {
            ui.animate();
        }
        ui.p.push_clip(area);
        let pitch = 25.0;
        let started = pos + 0.25 >= self.lyrics.first().map_or(0.0, |l| l.0);
        for k in -1i32..=2 {
            let i = idx as i32 + k;
            if i < 0 || i as usize >= self.lyrics.len() || (!started && k <= 0) {
                continue;
            }
            let text = self.lyrics[i as usize].1.trim();
            let text = if text.is_empty() { "♪" } else { text };
            let kf = k as f32 + (1.0 - e);
            let w = (1.0 - kf.abs()).clamp(0.0, 1.0);
            let alpha = if kf < 0.0 { w } else { 0.34 + 0.66 * w };
            let size = 14.5 + 4.5 * w;
            let style = TextStyle::new(size, Color::white(alpha)).weight(if w > 0.5 { 600 } else { 500 });
            ui.p.text(text, Rect::new(area.x, area.y + 1.0 + kf * pitch, area.w, 26.0), &style);
        }
        ui.p.pop_clip();
    }
}

/// Look the lyrics up on LRCLIB (free, no key). Only synced lyrics are used.
fn fetch_lyrics(title: &str, artist: &str, duration: f64) -> Option<Vec<(f64, String)>> {
    let (title, artist) = clean_track(title, artist);
    let enc = |s: &str| {
        let mut o = String::new();
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => o.push(b as char),
                _ => o.push_str(&format!("%{b:02X}")),
            }
        }
        o
    };
    let cancel = AtomicBool::new(false);
    let headers = [("User-Agent", "DynamicNotch (https://github.com/eigenes/DynamicNotch)".to_string())];
    let get = |url: &str| -> Option<serde_json::Value> {
        let (status, body) = http::fetch("GET", url, &headers, &[], Timeouts::DEFAULT, 1024 * 1024, &cancel).ok()?;
        if status != 200 {
            return None;
        }
        serde_json::from_slice(&body).ok()
    };
    let mut url = format!("https://lrclib.net/api/get?track_name={}&artist_name={}", enc(&title), enc(&artist));
    if duration > 1.0 {
        url.push_str(&format!("&duration={:.0}", duration));
    }
    let mut synced = get(&url).and_then(|v| v["syncedLyrics"].as_str().map(str::to_string));
    if synced.as_deref().is_none_or(|s| s.trim().is_empty()) {
        // fall back to a search and take the closest synced hit
        let v = get(&format!("https://lrclib.net/api/search?q={}", enc(&format!("{artist} {title}"))))?;
        synced = v.as_array()?.iter().find_map(|h| {
            let ok = duration < 1.0 || h["duration"].as_f64().is_none_or(|d| (d - duration).abs() < 6.0);
            h["syncedLyrics"].as_str().filter(|s| ok && !s.trim().is_empty()).map(str::to_string)
        });
    }
    let lines = parse_lrc(&synced?);
    (lines.len() > 2).then_some(lines)
}

/// "Song (feat. X) - Remastered 2011" → "Song"; "Artist - Topic" → "Artist".
fn clean_track(title: &str, artist: &str) -> (String, String) {
    let mut t = title.to_string();
    let mut a = artist.trim_end_matches(" - Topic").trim().to_string();
    if let Some((x, y)) = title.split_once(" - ") {
        // browsers often put "Artist - Song" in the title
        if a.is_empty() || x.trim().eq_ignore_ascii_case(&a) {
            a = x.trim().to_string();
            t = y.to_string();
        } else {
            t = x.to_string();
        }
    }
    let mut out = String::new();
    let mut depth = 0;
    for c in t.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = (depth - 1).max(0),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    (out.trim().to_string(), a)
}

fn parse_lrc(s: &str) -> Vec<(f64, String)> {
    let mut v = Vec::new();
    for line in s.lines() {
        let mut rest = line.trim();
        let mut stamps = Vec::new();
        while let Some(r) = rest.strip_prefix('[') {
            let Some((tag, after)) = r.split_once(']') else { break };
            if let Some((m, sec)) = tag.split_once(':') {
                if let (Ok(m), Ok(sec)) = (m.parse::<f64>(), sec.parse::<f64>()) {
                    stamps.push(m * 60.0 + sec);
                }
            }
            rest = after;
        }
        for t in stamps {
            v.push((t, rest.trim().to_string()));
        }
    }
    v.sort_by(|a, b| a.0.total_cmp(&b.0));
    v
}

fn worker(rx: Receiver<Cmd>, tx: Sender<Cmd>, bus: Bus) {
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
    let mgr = match Manager::RequestAsync().and_then(|op| op.join()) {
        Ok(m) => m,
        Err(e) => {
            crate::log!("media: session manager unavailable: {e}");
            return;
        }
    };
    let ev_tx = tx.clone();
    let _ = mgr.CurrentSessionChanged(&TypedEventHandler::new(move |_, _| {
        let _ = ev_tx.send(Cmd::Refresh);
        Ok(())
    }));
    let ev_tx = tx.clone();
    let _ = mgr.SessionsChanged(&TypedEventHandler::new(move |_, _| {
        let _ = ev_tx.send(Cmd::Refresh);
        Ok(())
    }));

    let mut session: Option<(Session, [i64; 3])> = None;
    let mut last_key = String::new();
    let mut last_art: Option<ImageData> = None;
    let mut refresh = true;

    loop {
        if refresh {
            refresh = false;
            // (re)bind to the current session's events
            let current = mgr.GetCurrentSession().ok();
            let same = match (&session, &current) {
                (Some((a, _)), Some(b)) => a == b,
                (None, None) => true,
                _ => false,
            };
            if !same {
                if let Some((s, t)) = session.take() {
                    let _ = s.RemoveMediaPropertiesChanged(t[0]);
                    let _ = s.RemovePlaybackInfoChanged(t[1]);
                    let _ = s.RemoveTimelinePropertiesChanged(t[2]);
                }
                if let Some(s) = current {
                    let t0 = s.MediaPropertiesChanged(&refresh_handler(&tx)).unwrap_or(0);
                    let t1 = s.PlaybackInfoChanged(&refresh_handler(&tx)).unwrap_or(0);
                    let t2 = s.TimelinePropertiesChanged(&refresh_handler(&tx)).unwrap_or(0);
                    session = Some((s, [t0, t1, t2]));
                }
            }
            let snap = match &session {
                Some((s, _)) => read_snapshot(s, &mut last_key, &mut last_art),
                None => {
                    last_key.clear();
                    last_art = None;
                    Snapshot::default()
                }
            };
            bus.to_module(ID, snap);
        }

        // wait for the next event / command; coalesce bursts of events
        let cmd = match rx.recv() {
            Ok(c) => c,
            Err(_) => return,
        };
        let mut cmds = vec![cmd];
        loop {
            match rx.recv_timeout(Duration::from_millis(40)) {
                Ok(c) => cmds.push(c),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        for c in cmds {
            let s = session.as_ref().map(|(s, _)| s);
            match c {
                Cmd::Refresh => refresh = true,
                Cmd::Quit => return,
                Cmd::PlayPause => {
                    if let Some(s) = s {
                        let _ = s.TryTogglePlayPauseAsync().and_then(|o| o.join());
                    }
                    refresh = true;
                }
                Cmd::Next => {
                    if let Some(s) = s {
                        let _ = s.TrySkipNextAsync().and_then(|o| o.join());
                    }
                }
                Cmd::Prev => {
                    if let Some(s) = s {
                        let _ = s.TrySkipPreviousAsync().and_then(|o| o.join());
                    }
                }
                Cmd::Seek(secs) => {
                    if let Some(s) = s {
                        let ticks = (secs * 10_000_000.0) as i64;
                        let _ = s.TryChangePlaybackPositionAsync(ticks).and_then(|o| o.join());
                    }
                    refresh = true;
                }
            }
        }
    }
}

fn refresh_handler<S, A>(tx: &Sender<Cmd>) -> TypedEventHandler<S, A>
where
    S: windows::core::RuntimeType + 'static,
    A: windows::core::RuntimeType + 'static,
{
    let t = tx.clone();
    TypedEventHandler::new(move |_, _| {
        let _ = t.send(Cmd::Refresh);
        Ok(())
    })
}

fn read_snapshot(s: &Session, last_key: &mut String, last_art: &mut Option<ImageData>) -> Snapshot {
    let mut snap = Snapshot { active: true, ..Default::default() };
    snap.app = friendly_app(&s.SourceAppUserModelId().map(|h| h.to_string()).unwrap_or_default());
    if let Ok(props) = s.TryGetMediaPropertiesAsync().and_then(|o| o.join()) {
        snap.title = props.Title().map(|h| h.to_string()).unwrap_or_default();
        snap.artist = props.Artist().map(|h| h.to_string()).unwrap_or_default();
        let key = format!("{}\u{1}{}\u{1}{}", snap.app, snap.title, snap.artist);
        if key != *last_key || last_art.is_none() {
            *last_art = props.Thumbnail().ok().and_then(|t| load_thumbnail(&t));
            *last_key = key;
        }
        snap.art = last_art.clone();
    }
    if let Ok(info) = s.GetPlaybackInfo() {
        snap.playing = info.PlaybackStatus().map(|st| st == Status::Playing).unwrap_or(false);
        if let Ok(c) = info.Controls() {
            snap.can_next = c.IsNextEnabled().unwrap_or(false);
            snap.can_prev = c.IsPreviousEnabled().unwrap_or(false);
            snap.can_seek = c.IsPlaybackPositionEnabled().unwrap_or(false);
        }
    }
    if let Ok(tl) = s.GetTimelineProperties() {
        let start = tl.StartTime().map(|t| t.Duration).unwrap_or(0);
        let end = tl.EndTime().map(|t| t.Duration).unwrap_or(0);
        let pos = tl.Position().map(|t| t.Duration).unwrap_or(0);
        snap.duration = ((end - start) as f64 / 1e7).max(0.0);
        let mut p = (pos - start) as f64 / 1e7;
        // The timeline is a snapshot from `LastUpdatedTime`; extrapolate.
        if snap.playing {
            if let Ok(updated) = tl.LastUpdatedTime() {
                let now = unsafe { GetSystemTimeAsFileTime() };
                let now = ((now.dwHighDateTime as i64) << 32) | now.dwLowDateTime as i64;
                let dt = (now - updated.UniversalTime) as f64 / 1e7;
                if (0.0..3600.0).contains(&dt) {
                    p += dt;
                }
            }
        }
        snap.position = p.max(0.0);
    }
    snap
}

fn load_thumbnail(r: &windows::Storage::Streams::IRandomAccessStreamReference) -> Option<ImageData> {
    let stream = r.OpenReadAsync().ok()?.join().ok()?;
    let size = stream.Size().ok()? as u32;
    if size == 0 || size > 16 * 1024 * 1024 {
        return None;
    }
    let reader = DataReader::CreateDataReader(&stream).ok()?;
    let op: IAsyncOperation<u32> = reader.LoadAsync(size).ok()?.cast().ok()?;
    let n = op.join().ok()?;
    let mut buf = vec![0u8; n as usize];
    reader.ReadBytes(&mut buf).ok()?;
    image::decode(&buf, 256).map(crop_letterbox)
}

/// Browser thumbnails are often 16:9 with the square cover in the middle
/// and flat bars on the sides; crop to the center square in that case.
fn crop_letterbox(img: ImageData) -> ImageData {
    let (w, h) = (img.width, img.height);
    if w <= h + h / 10 {
        return img;
    }
    let side = h;
    let x0 = (w - side) / 2;
    let mut out = Vec::with_capacity((side * side * 4) as usize);
    for y in 0..side {
        let row = ((y * w + x0) * 4) as usize;
        out.extend_from_slice(&img.pixels[row..row + (side * 4) as usize]);
    }
    ImageData::from_pbgra(side, side, out)
}

fn friendly_app(aumid: &str) -> String {
    // Packaged apps: "Publisher.AppName_hash!EntryPoint" → use the package name.
    let s = aumid.split_once('!').map_or(aumid, |(pkg, _)| pkg);
    let s = s.trim_end_matches(".exe").trim_end_matches(".EXE");
    let s = s.split('_').next().unwrap_or(s);
    let s = s.rsplit('.').next().unwrap_or(s);
    let lower = s.to_ascii_lowercase();
    match lower.as_str() {
        "msedge" => "Microsoft Edge".into(),
        "chrome" => "Chrome".into(),
        "firefox" => "Firefox".into(),
        "spotify" | "spotifymusic" => "Spotify".into(),
        "zunemusic" | "mediaplayer" => "Media Player".into(),
        "vlc" => "VLC".into(),
        _ => {
            let mut c = s.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        }
    }
}
