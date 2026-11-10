//! The volume mixer: the main volume and one slider for every program that makes sound.
//!
//! It is a hidden page of the notch. It has no tab, no page dot and no hotkey: the only way in is a click on
//! the volume text of the volume peek (the value in the round dial). A click outside the notch closes it.
//!
//! Cost: nothing exists until the page is first drawn. Then one worker thread (asleep on a channel) reads and
//! sets the sessions of the default output through the Windows audio session API. While the page is on screen
//! it is asked for a fresh list every 1.5 s; once the page has been away for a moment the asking stops.
//! Frames only come while something moves.

use std::any::Any;
use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Win32::Media::Audio::{
    eConsole, eRender, AudioSessionStateExpired, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator,
    ISimpleAudioVolume, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};

use super::{Cx, Module, ModuleId};
use crate::anim::ease_out;
use crate::bus::Bus;
use crate::gfx::{Icon, ImageData, TextStyle};
use crate::ui::{gkey, Ui};
use crate::util::{palette, Color, Rect};

pub const ID: ModuleId = "mixer";

/// One row: a program, or the main volume.
const ROW: f32 = 46.0;
/// Program rows that show at once; more scroll with the wheel.
const VIS: usize = 4;
/// The caption line on top.
const HEAD: f32 = 26.0;
/// How often the list is read while the page is open.
const SCAN_EVERY: Duration = Duration::from_millis(1500);
/// A page that was not drawn for this long counts as closed (and opens with its small entrance again).
const AWAY: Duration = Duration::from_millis(2500);
/// A dragged slider tells Windows at most this often (the last value is always sent when it is let go).
const SEND_EVERY: Duration = Duration::from_millis(50);

/// A program that has a sound session on the default output.
#[derive(Clone)]
struct AppRow {
    /// What identifies it: the lower-case path of its program ("system" for the Windows sounds).
    key: String,
    name: String,
    level: f32,
    muted: bool,
    icon: Option<ImageData>,
}

enum Cmd {
    Scan,
    Set(String, f32),
    Mute(String, bool),
    Quit,
}

/// The worker's answer to a scan.
struct Scanned(Vec<AppRow>);

pub struct Mixer {
    tx: Option<Sender<Cmd>>,
    rows: Vec<AppRow>,
    /// A list has arrived at least once (so "nothing is playing" is not shown too early).
    loaded: bool,
    asked: Option<Instant>,
    seen: Option<Instant>,
    opened: Instant,
    /// Scroll target of the program rows.
    off: f32,
    /// The program slider being dragged: its key and the value it was dragged to.
    drag: Option<(String, f32)>,
    sent: Instant,
}

impl Mixer {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            tx: None,
            rows: Vec::new(),
            loaded: false,
            asked: None,
            seen: None,
            opened: now,
            off: 0.0,
            drag: None,
            sent: now,
        }
    }

    /// Hand a job to the worker (which is started by the first one).
    fn send(&mut self, bus: &Bus, cmd: Cmd) {
        if self.tx.is_none() {
            let (tx, rx) = channel();
            let b = bus.clone();
            let _ = std::thread::Builder::new().name("mixer".into()).spawn(move || worker(rx, b));
            self.tx = Some(tx);
        }
        if let Some(tx) = &self.tx {
            let _ = tx.send(cmd);
        }
    }
}

impl Module for Mixer {
    fn id(&self) -> ModuleId {
        ID
    }
    fn title(&self) -> &str {
        "Volume mixer"
    }
    fn icon(&self) -> Icon {
        Icon::Volume
    }

    fn stop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Cmd::Quit);
        }
    }

    fn on_message(&mut self, msg: Box<dyn Any + Send>, cx: &mut Cx) {
        let Ok(m) = msg.downcast::<Scanned>() else { return };
        let mut rows = m.0;
        // the slider under the pointer keeps the value it is being dragged to
        if let Some((k, v)) = &self.drag {
            if let Some(r) = rows.iter_mut().find(|r| r.key == *k) {
                r.level = *v;
                r.muted = false;
            }
        }
        self.rows = rows;
        self.loaded = true;
        cx.fx.redraw = true;
    }

    /// Ask again while the page is on screen, and only then.
    fn next_tick(&self) -> Option<Instant> {
        let seen = self.seen?;
        if seen.elapsed() > AWAY {
            return None;
        }
        Some(self.asked.map_or_else(Instant::now, |a| a + SCAN_EVERY))
    }

    fn on_tick(&mut self, cx: &mut Cx) {
        if self.seen.is_some_and(|t| t.elapsed() <= AWAY) {
            self.asked = Some(Instant::now());
            self.send(cx.bus, Cmd::Scan);
        }
    }

    /// Reachable only through the volume peek, so it never shows as a tab.
    fn has_page(&self) -> bool {
        true
    }

    fn page_height(&self, _width: f32) -> f32 {
        let body = if self.rows.is_empty() { ROW } else { self.rows.len().min(VIS) as f32 * ROW };
        HEAD + ROW + 6.0 + body + 4.0
    }

    fn draw_page(&mut self, ui: &mut Ui, r: Rect) {
        let now = ui.now;
        // a fresh visit: the rows come in again from the top of the list
        if self.seen.is_none_or(|t| now.saturating_duration_since(t) > AWAY) {
            self.opened = now;
            self.off = 0.0;
            self.drag = None;
            self.asked = None;
        }
        self.seen = Some(now);
        if self.asked.is_none() {
            self.asked = Some(now);
            self.send(ui.bus, Cmd::Scan);
        }
        let t = now.saturating_duration_since(self.opened).as_secs_f32();
        let a0 = ui.p.alpha;
        let mut busy = false;

        ui.p.text(
            "Volume mixer",
            Rect::new(r.x + 6.0, r.y, r.w - 12.0, HEAD - 4.0),
            &TextStyle::new(12.5, palette::TEXT_DIM).medium(),
        );

        // the main volume
        let mut y = r.y + HEAD;
        let rk = ease_out((t / 0.26).clamp(0.0, 1.0));
        if rk < 1.0 {
            busy = true;
        }
        let level = super::osd::volume_now().unwrap_or(0.0);
        let icon = if level < 0.005 { Icon::Mute } else { Icon::Volume };
        ui.p.alpha = a0 * rk;
        let row = Rect::new(r.x, y + (1.0 - rk) * 10.0, r.w, ROW - 4.0);
        let hit = slider_row(ui, row, "Main volume", None, icon, level, false, "mix-main", false);
        if let Some(v) = hit.drag.or(hit.tap) {
            ui.fx.set_volume = Some(v);
        }

        // a faint line, then the programs
        y += ROW + 6.0;
        ui.p.alpha = a0 * rk * 0.6;
        ui.p.fill_rounded(Rect::new(r.x + 10.0, y - 4.0, r.w - 20.0, 1.0), 0.5, Color::white(0.10));

        let n = self.rows.len();
        let area = Rect::new(r.x, y, r.w, if n == 0 { ROW } else { n.min(VIS) as f32 * ROW });
        let mut dragging = false;
        if n == 0 {
            if self.loaded {
                let k = ease_out(((t - 0.1) / 0.26).clamp(0.0, 1.0));
                if k < 1.0 {
                    busy = true;
                }
                ui.p.alpha = a0 * k;
                ui.p.text(
                    "No program is playing sound",
                    area,
                    &TextStyle::new(13.0, palette::TEXT_FAINT).medium().center(),
                );
            }
        } else {
            let off = if n > VIS {
                crate::alcove::scroll_rows(ui, area, &mut self.off, n as f32 * ROW, ROW, "mixer-rows")
            } else {
                0.0
            };
            ui.p.push_clip(area);
            for i in 0..n {
                let ry = area.y + i as f32 * ROW - off;
                if ry + ROW < area.y || ry > area.bottom() {
                    continue;
                }
                let k = ease_out(((t - 0.05 - i.min(8) as f32 * 0.035) / 0.26).clamp(0.0, 1.0));
                if k < 1.0 {
                    busy = true;
                }
                ui.p.alpha = a0 * k;
                let row = Rect::new(area.x, ry + (1.0 - k) * 10.0, area.w, ROW - 4.0);
                let hit = {
                    let a = &self.rows[i];
                    slider_row(ui, row, &a.name, a.icon.as_ref(), Icon::Volume, a.level, a.muted, &a.key, true)
                };
                if let Some(v) = hit.drag {
                    dragging = true;
                    self.rows[i].level = v;
                    self.rows[i].muted = false;
                    let key = self.rows[i].key.clone();
                    if now.saturating_duration_since(self.sent) >= SEND_EVERY {
                        self.sent = now;
                        self.send(ui.bus, Cmd::Set(key.clone(), v));
                    }
                    self.drag = Some((key, v));
                } else if let Some(v) = hit.tap {
                    self.rows[i].level = v;
                    self.rows[i].muted = false;
                    let key = self.rows[i].key.clone();
                    self.send(ui.bus, Cmd::Set(key, v));
                }
                if hit.mute {
                    let m = !self.rows[i].muted;
                    self.rows[i].muted = m;
                    let key = self.rows[i].key.clone();
                    self.send(ui.bus, Cmd::Mute(key, m));
                }
            }
            ui.p.pop_clip();
        }
        // the slider was let go: the last value goes out for sure
        if !dragging {
            if let Some((key, v)) = self.drag.take() {
                self.send(ui.bus, Cmd::Set(key, v));
            }
        }
        ui.p.alpha = a0;
        if busy {
            ui.animate();
        }
    }
}

/// What happened on a row this frame.
struct Hit {
    /// The value the slider is being dragged to.
    drag: Option<f32>,
    /// A click on the track (a value).
    tap: Option<f32>,
    /// A tap on the picture (mute or unmute).
    mute: bool,
}

/// One row: the picture, the name, a capsule slider and the percentage. `tag` keeps the eased values
/// of this row apart from the others.
#[allow(clippy::too_many_arguments)]
fn slider_row(
    ui: &mut Ui,
    row: Rect,
    name: &str,
    img: Option<&ImageData>,
    fallback: Icon,
    level: f32,
    muted: bool,
    tag: &str,
    can_mute: bool,
) -> Hit {
    if ui.hovered(row) {
        ui.hot = true;
        ui.p.fill_rounded(row, 14.0, Color::white(0.05));
    }
    // the picture; a tap on it mutes the program
    let ib = Rect::new(row.x + 10.0, row.cy() - 14.0, 28.0, 28.0);
    let a = ui.p.alpha;
    if muted {
        ui.p.alpha = a * 0.4;
    }
    match img {
        Some(img) => ui.p.image(img, ib, 7.0, 1.0),
        None => {
            ui.p.fill_rounded(ib, 9.0, Color::white(0.10));
            ui.p.icon(fallback, ib.inset(6.0), Color::white(0.9));
        }
    }
    ui.p.alpha = a;
    let tap_zone = ib.expand(4.0);
    if can_mute && ui.hovered(tap_zone) {
        ui.hot = true;
    }
    let mute = can_mute && ui.clicked(tap_zone);

    // the name
    let tx = ib.right() + 12.0;
    let name_w = (row.w * 0.27).clamp(86.0, 140.0);
    ui.p.push_clip(Rect::new(tx, row.y, name_w, row.h));
    ui.p.text(name, Rect::new(tx, row.y, name_w + 40.0, row.h), &TextStyle::new(13.5, Color::white(0.92)).medium());
    ui.p.pop_clip();

    // the slider
    let x0 = tx + name_w + 8.0;
    let track = Rect::new(x0, row.cy() - 5.0, (row.right() - 56.0 - x0).max(40.0), 10.0);
    let hit = Rect::new(track.x - 8.0, track.y - 14.0, track.w + 16.0, track.h + 28.0);
    let mut drag = None;
    if ui.pressed(hit) {
        if let Some((mx, _)) = ui.input.mouse {
            drag = Some(((mx - track.x) / track.w).clamp(0.0, 1.0));
        }
        ui.animate();
    }
    let tap = if drag.is_none() && ui.clicked(hit) {
        ui.input.click.map(|(_, (ux, _))| ((ux - track.x) / track.w).clamp(0.0, 1.0))
    } else {
        None
    };
    let hot = ui.hovered(hit) || drag.is_some();
    if hot {
        ui.hot = true;
    }
    let grow = ui.glide(gkey(tag, 1), if hot { 1.0 } else { 0.0 });
    let shown = match drag.or(tap) {
        Some(v) => v,
        None => ui.glide(gkey(tag, 2), level.clamp(0.0, 1.0)),
    };
    let th = 10.0 + 2.0 * grow;
    let tr = Rect::new(track.x, track.cy() - th * 0.5, track.w, th);
    ui.p.fill_rounded(tr, th * 0.5, Color::white(0.14));
    let fill = if muted { Color::white(0.35) } else { Color::white(0.97) };
    ui.p.fill_rounded(Rect::new(tr.x, tr.y, (tr.w * shown).max(th), th), th * 0.5, fill);

    // the percentage
    let label = if muted { "Muted".to_string() } else { format!("{:.0}%", shown * 100.0) };
    ui.p.text(
        &label,
        Rect::new(row.right() - 54.0, row.y, 46.0, row.h),
        &TextStyle::new(13.0, palette::TEXT_DIM).medium().right(),
    );
    Hit { drag, tap, mute }
}

// -- the worker ---------------------------------------------------------------------------------------------

fn worker(rx: Receiver<Cmd>, bus: Bus) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let en: IMMDeviceEnumerator = match CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) {
            Ok(e) => e,
            Err(e) => {
                crate::log!("mixer: audio devices unavailable: {e}");
                return;
            }
        };
        // looking up a program's picture takes a moment: once per program
        let mut icons: HashMap<String, Option<ImageData>> = HashMap::new();
        while let Ok(cmd) = rx.recv() {
            match cmd {
                Cmd::Quit => break,
                Cmd::Scan => bus.to_module(ID, Scanned(scan(&en, &mut icons))),
                Cmd::Set(key, v) => {
                    for (k, _, vol) in sessions(&en) {
                        if k == key {
                            let _ = vol.SetMute(false, std::ptr::null());
                            let _ = vol.SetMasterVolume(v.clamp(0.0, 1.0), std::ptr::null());
                        }
                    }
                }
                Cmd::Mute(key, m) => {
                    for (k, _, vol) in sessions(&en) {
                        if k == key {
                            let _ = vol.SetMute(m, std::ptr::null());
                        }
                    }
                }
            }
        }
    }
}

/// The sessions of the default output that are not expired: key, program path and the volume control.
/// A program with several sessions (a browser) shows up once per session; `scan` and the setters group them.
unsafe fn sessions(en: &IMMDeviceEnumerator) -> Vec<(String, String, ISimpleAudioVolume)> {
    let mut out = Vec::new();
    let Ok(dev) = en.GetDefaultAudioEndpoint(eRender, eConsole) else { return out };
    let Ok(mgr) = dev.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else { return out };
    let Ok(list) = mgr.GetSessionEnumerator() else { return out };
    for i in 0..list.GetCount().unwrap_or(0) {
        let Ok(ctl) = list.GetSession(i) else { continue };
        if ctl.GetState().is_ok_and(|s| s == AudioSessionStateExpired) {
            continue;
        }
        let Ok(c2) = ctl.cast::<IAudioSessionControl2>() else { continue };
        let Ok(vol) = ctl.cast::<ISimpleAudioVolume>() else { continue };
        if c2.IsSystemSoundsSession().0 == 0 {
            out.push(("system".to_string(), String::new(), vol));
            continue;
        }
        let exe = crate::dock::exe_of(c2.GetProcessId().unwrap_or(0));
        if exe.is_empty() {
            continue;
        }
        out.push((exe.to_ascii_lowercase(), exe, vol));
    }
    out
}

/// The rows to show: one per program, in a steady order (so nothing jumps around while it is open).
unsafe fn scan(en: &IMMDeviceEnumerator, icons: &mut HashMap<String, Option<ImageData>>) -> Vec<AppRow> {
    let mut rows: Vec<AppRow> = Vec::new();
    for (key, exe, vol) in sessions(en) {
        let level = vol.GetMasterVolume().unwrap_or(1.0);
        let muted = vol.GetMute().is_ok_and(|m| m.as_bool());
        if let Some(r) = rows.iter_mut().find(|r| r.key == key) {
            r.level = r.level.max(level);
            r.muted &= muted;
            continue;
        }
        let name = if exe.is_empty() { "System sounds".to_string() } else { nice_name(&exe) };
        let icon = icons
            .entry(key.clone())
            .or_insert_with(|| if exe.is_empty() { None } else { crate::dock::icon_for_path(&exe, &name) })
            .clone();
        rows.push(AppRow { key, name, level, muted, icon });
    }
    rows.sort_by_key(|r| r.name.to_lowercase());
    rows
}

/// "C:\...\spotify.exe" → "Spotify".
fn nice_name(exe: &str) -> String {
    let file = exe.rsplit('\\').next().unwrap_or(exe);
    let stem = file.rsplit_once('.').map_or(file, |(s, _)| s);
    match stem.to_ascii_lowercase().as_str() {
        "msedge" => return "Microsoft Edge".to_string(),
        "chrome" => return "Google Chrome".to_string(),
        _ => {}
    }
    let mut s = stem.replace(['_', '-'], " ");
    let first = s.get(..1).map(|c| c.to_uppercase());
    if let Some(up) = first {
        s.replace_range(..1, &up);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::nice_name;

    #[test]
    fn names_of_programs() {
        assert_eq!(nice_name(r"C:\Users\a\AppData\Roaming\Spotify\Spotify.exe"), "Spotify");
        assert_eq!(nice_name(r"C:\Program Files\Microsoft\Edge\Application\msedge.exe"), "Microsoft Edge");
        assert_eq!(nice_name(r"C:\x\vlc_player.exe"), "Vlc player");
    }
}
