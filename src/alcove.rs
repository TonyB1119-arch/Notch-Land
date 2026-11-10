//! The Alcove look's own notch screens: a home page with the weather on the left and a big
//! clock beside it, and a column of page dots on the right edge. The dots grow a little under the
//! pointer and a click on one opens that page; the wheel over them steps through the pages.
//!
//! Everything is drawn with the same primitives as the other looks.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::gfx::{Icon, TextStyle};
use crate::modules::{Module, ModuleId};
use crate::ui::{gkey, Ui};
use crate::util::{palette, Color, Rect};

/// Space above, left of, right of and under the page content.
pub const TOP: f32 = 26.0;
pub const LEFT: f32 = 30.0;
pub const RIGHT: f32 = 54.0;
pub const BOTTOM: f32 = 26.0;
/// Height of the home page.
pub const HOME_H: f32 = 118.0;
/// The home page while the mirror's camera view is open: taller, so the picture has room.
pub const HOME_MIRROR_H: f32 = 200.0;
/// The even gap between the clock and the two small widgets beside it: as wide as there is room
/// for, but never wider than GAP, and the sky label leaves the date line before it gets under MIN_GAP.
const GAP: f32 = 32.0;
const MIN_GAP: f32 = 22.0;

/// How many dots show at once; the window follows the open page.
const MAX_DOTS: usize = 7;

// ---------------------------------------------------------------------------
// Home
// ---------------------------------------------------------------------------

/// How far the notch has opened (0..1), set once per frame so the home page can bring its parts in
/// one after another while it opens.
static OPEN_PROGRESS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1000);

pub fn set_open(p: f32) {
    OPEN_PROGRESS.store((p.clamp(0.0, 1.0) * 1000.0) as u32, std::sync::atomic::Ordering::Relaxed);
}

/// Opacity of the `i`-th part of a page while it opens: each starts a little after the one before.
fn stage(i: usize) -> f32 {
    let p = OPEN_PROGRESS.load(std::sync::atomic::Ordering::Relaxed) as f32 / 1000.0;
    if p >= 0.999 {
        return 1.0;
    }
    let t = ((p - 0.14 * i as f32) / 0.55).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

pub fn home(ui: &mut Ui, full: Rect, modules: &mut [Box<dyn Module>]) {
    // the widgets keep one compact size and stay at the top; only the camera view grows past them
    let r = Rect::new(full.x, full.y, full.w, HOME_H.min(full.h));
    let base = ui.p.alpha;
    let mirror_on = ui.cfg.modules.is_enabled("mirror");
    let covered = mirror_on && crate::modules::mirror::showing();
    // settings gear, top left (the mirror's camera icon sits right beside it)
    let gear = Rect::new(r.x - 4.0, r.y - 8.0, 22.0, 22.0);
    let hover = !covered && ui.hovered(gear);
    if hover {
        ui.hot = true;
    }
    let a = ui.glide(gkey("alcove-gear", 0), if hover { 1.0 } else { 0.0 });
    ui.p.icon(Icon::Settings, gear, Color::white(0.45 + 0.45 * a));
    if !covered && ui.clicked(gear) {
        ui.fx.open_page = Some("settings");
    }

    let brief = crate::modules::weather::brief();
    let unit = if ui.cfg.weather.units == "imperial" { "F" } else { "C" };

    // the three parts are measured first, so the gaps between them can be even
    // left: the weather
    let temp_style = TextStyle::new(30.0, Color::white(1.0)).semibold();
    let place_style = TextStyle::new(13.0, palette::TEXT_DIM);
    let off_style = TextStyle::new(13.0, palette::TEXT_FAINT);
    let (temp_txt, place) = match &brief {
        Some(b) => {
            let mut place = b.place.clone();
            if place.chars().count() > 18 {
                place = place.chars().take(17).collect::<String>() + "…";
            }
            (format!("{:.0}°{unit}", b.temp), place)
        }
        None => (String::new(), String::new()),
    };
    let temp_w = ui.p.measure(&temp_txt, &temp_style).0;
    let place_w = ui.p.measure(&place, &place_style).0;
    let left_w = if brief.is_some() {
        (50.0 + temp_w).max(place_w)
    } else {
        46.0 + ui.p.measure("Weather is off", &off_style).0
    };
    // right: the readings
    let (rows, lab_w, val_w) = stats_layout(ui, modules);
    let right_w = lab_w + 12.0 + val_w;
    // centre: the time and the date, in the middle of the notch
    let (h, m, _s, suffix, date) = crate::app::clock_parts();
    let time = format!("{h:02}:{m:02}");
    let big = TextStyle::new(48.0, Color::white(1.0)).weight(700).center();
    let suffix_style = TextStyle::new(14.0, palette::TEXT_DIM).semibold();
    let (tw, _) = ui.p.measure(&time, &big);
    let (sw, _) = if suffix.is_empty() { (0.0, 0.0) } else { ui.p.measure(&suffix, &suffix_style) };
    let gap = if suffix.is_empty() { 0.0 } else { 7.0 };
    let clock_w = tw + gap + sw;
    let sub_style = TextStyle::new(14.0, palette::TEXT_DIM).medium().center();
    let mut sub = match &brief {
        Some(b) => format!("{date}  ·  {}", b.label),
        None => date.clone(),
    };
    let cx = r.cx() + (RIGHT - LEFT) * 0.5;
    let room = |block: f32| ((cx - block * 0.5) - r.x - left_w).min(r.right() - (cx + block * 0.5) - right_w);
    let mut block = clock_w.max(ui.p.measure(&sub, &sub_style).0);
    if room(block) < MIN_GAP {
        // the sky label gives way before anything gets crowded
        sub = date;
        block = clock_w.max(ui.p.measure(&sub, &sub_style).0);
    }
    let g = room(block).clamp(14.0, GAP);
    let wx = cx - block * 0.5 - g - left_w;
    let sx = cx + block * 0.5 + g;

    // draw: the weather
    ui.p.alpha = base * stage(1);
    let wy = r.cy() - 35.0;
    match &brief {
        Some(b) => {
            b.draw_icon(ui.p, Rect::new(wx, wy, 40.0, 40.0));
            ui.p.text(&temp_txt, Rect::new(wx + 50.0, wy - 2.0, temp_w + 8.0, 44.0), &temp_style);
            ui.p.text(&place, Rect::new(wx, wy + 48.0, place_w + 8.0, 20.0), &place_style);
        }
        None => {
            ui.p.icon(Icon::Cloud, Rect::new(wx, wy, 36.0, 36.0), Color::white(0.3));
            ui.p.text("Weather is off", Rect::new(wx + 46.0, wy, left_w, 36.0), &off_style);
        }
    }
    // the clock and the date
    ui.p.alpha = base * stage(0);
    let left = cx - clock_w * 0.5;
    let ty = r.cy() - 51.0;
    ui.p.text(&time, Rect::new(left - 4.0, ty, tw + 8.0, 62.0), &big);
    if !suffix.is_empty() {
        ui.p.text(&suffix, Rect::new(left + tw + gap, ty + 19.0, sw + 8.0, 24.0), &suffix_style);
    }
    ui.p.text(&sub, Rect::new(cx - block * 0.5 - 20.0, r.cy() + 17.0, block + 40.0, 24.0), &sub_style);
    // the readings
    ui.p.alpha = base * stage(2);
    side_stats(ui, r, sx, &rows, lab_w, val_w);
    ui.p.alpha = base;

    // the mirror: a camera icon next to the gear that opens a camera view over the page
    if mirror_on {
        if let Some(mi) = modules.iter_mut().find(|m| m.id() == "mirror") {
            mi.draw_card(ui, full);
        }
    }
}

// ---------------------------------------------------------------------------
// Page dots
// ---------------------------------------------------------------------------

/// The column of page dots on the right edge. `view` is the notch as it is right now, so the dots
/// ride its edge (and middle) while it resizes, growing or shrinking, instead of jumping.
pub fn rail(ui: &mut Ui, view: Rect, tabs: &[(ModuleId, Icon, String)], page: ModuleId) {
    if tabs.len() < 2 {
        return;
    }
    let n = tabs.len();
    let cur = tabs.iter().position(|t| t.0 == page).unwrap_or(0);

    let shown = n.min(MAX_DOTS);
    let first = if n <= MAX_DOTS { 0 } else { cur.saturating_sub(shown / 2).min(n - shown) };
    let pitch = 16.0;
    let col_h = pitch * shown as f32;
    let right = view.right();
    let cy = view.cy();
    let zone = Rect::new(right - 46.0, cy - col_h * 0.5 - 14.0, 46.0, col_h + 28.0);

    // a gentle swell under the pointer
    let over = ui.hovered(zone);
    let want = if over { 1.0 } else { 0.0 };
    let t = ui.glide(gkey("alcove-rail", 0), want);
    if (t - want).abs() > 0.01 {
        ui.animate();
    }
    let grow = 1.0 + 0.22 * t;

    // the dots scroll with the window and the white pill glides to the open page
    let fo = ui.glide(gkey("alcove-first", 0), first as f32);
    let cp = ui.glide(gkey("alcove-pos", 0), cur as f32);
    let x = right - 22.0;
    let scrolling = n > MAX_DOTS;
    for (i, tab) in tabs.iter().enumerate() {
        let k = i as f32 - fo;
        if k < -0.6 || k > shown as f32 - 0.4 {
            continue;
        }
        let y = zone.y + 14.0 + pitch * (k + 0.5);
        let edge = if scrolling { (k + 0.5).min(shown as f32 - 0.5 - k).clamp(0.0, 1.0) } else { 1.0 };
        let hw = 3.0 * (0.55 + 0.45 * edge) * grow;
        ui.p.fill_rounded(Rect::new(x - hw, y - hw, hw * 2.0, hw * 2.0), hw, Color::white(0.28 * edge));
        if edge > 0.5 && tab.0 != page && ui.clicked(Rect::new(x - 12.0, y - pitch * 0.5, 24.0, pitch)) {
            ui.fx.open_page = Some(tab.0);
        }
    }
    let py = zone.y + 14.0 + pitch * (cp - fo + 0.5);
    let hh = (7.5 + 3.0 * (cur as f32 - cp).abs().min(1.0)) * grow;
    ui.p.fill_rounded(Rect::new(x - 3.0 * grow, py - hh, 6.0 * grow, hh * 2.0), 3.0 * grow, Color::white(0.96));
    if over {
        ui.hot = true;
    }
}

/// Processor, memory and battery as a column on the right of the home page: each a small
/// label with its value and a thin capsule gauge.
/// The readings of the home page (name, number, warn) and the widths of the name and number columns.
/// The numbers carry no percent sign; the column is the same width whatever the values are.
fn stats_layout(ui: &mut Ui, modules: &[Box<dyn Module>]) -> (Vec<(&'static str, String, bool)>, f32, f32) {
    let mut inds = Vec::new();
    for m in modules.iter().filter(|m| m.id() == "battery") {
        m.indicators(&mut inds);
    }
    let battery = inds.iter().find_map(|i| i.battery);
    let rows = vec![
        ("Processor", crate::modules::stats::cpu_percent().map_or("--".to_string(), |v| v.to_string()), false),
        ("Memory", crate::modules::stats::ram_percent().map_or("--".to_string(), |v| v.to_string()), false),
        match battery {
            Some((level, charging)) => (
                if charging { "Charging" } else { "Battery" },
                format!("{:.0}", level * 100.0),
                level < 0.2 && !charging,
            ),
            None => ("Battery", "AC".to_string(), false),
        },
    ];
    let label = TextStyle::new(14.0, palette::TEXT_DIM).medium();
    let value = TextStyle::new(17.0, Color::white(1.0)).semibold().right();
    let lab_w = rows.iter().map(|(n, _, _)| ui.p.measure(n, &label).0).fold(0.0, f32::max);
    let min_w = ui.p.measure("100", &value).0;
    let val_w = rows.iter().map(|(_, t, _)| ui.p.measure(t, &value).0).fold(min_w, f32::max);
    (rows, lab_w, val_w)
}

/// The readings, with their left edge at `x`: the name, a short gap, the number lined up on the right.
fn side_stats(ui: &mut Ui, r: Rect, x: f32, rows: &[(&'static str, String, bool)], lab_w: f32, val_w: f32) {
    let pitch = 30.0;
    let mut y = r.cy() - pitch * 1.5 + 4.0;
    let label = TextStyle::new(14.0, palette::TEXT_DIM).medium();
    let value = TextStyle::new(17.0, Color::white(1.0)).semibold().right();
    for (name, txt, warn) in rows {
        ui.p.text(name, Rect::new(x, y, lab_w + 6.0, 24.0), &label);
        let mut v = value;
        if *warn {
            v.color = palette::RED;
        }
        ui.p.text(txt, Rect::new(x + lab_w + 6.0, y, val_w + 6.0, 24.0), &v);
        y += pitch;
    }
}

/// A battery in the Alcove style: a rounded capsule that fills with the charge, a small
/// cap on the right and a bolt while charging.
pub fn battery_glyph(ui: &mut Ui, r: Rect, level: f32, charging: bool) {
    // a squircle: nearly as tall as it is wide, with generously rounded corners
    let bw = (r.h * 1.7).min(r.w - 4.0);
    let body = Rect::new(r.x + (r.w - 4.0 - bw) * 0.5, r.y, bw, r.h);
    let col = crate::app::battery_color(level, charging);
    let rad = body.h * 0.36;
    ui.p.fill_rounded(body, rad, Color::white(0.16));
    let inner = body.inset(2.5);
    let f = level.clamp(0.0, 1.0);
    ui.p.fill_rounded(Rect::new(inner.x, inner.y, (inner.w * f).max(inner.h), inner.h), (rad - 2.5).max(2.0), col);
    ui.p.fill_rounded(Rect::new(body.right() + 1.5, r.cy() - 3.0, 2.5, 6.0), 1.25, Color::white(0.35));
    if charging {
        ui.p.icon(Icon::Bolt, Rect::centered(body.cx(), body.cy(), body.h * 0.8, body.h * 0.8), Color::black(0.78));
    }
}

/// Four rising bars for a Wi-Fi signal strength (0..=4).
pub fn signal_bars(ui: &mut Ui, r: Rect, bars: u8, color: Color) {
    let w = r.w / 7.0;
    for k in 0..4u8 {
        let h = r.h * (0.34 + 0.22 * k as f32);
        let x = r.x + k as f32 * w * 1.7;
        let on = k < bars;
        ui.p.fill_rounded(Rect::new(x, r.bottom() - h, w, h), w * 0.5, if on { color } else { Color::white(0.18) });
    }
}

/// The strip at the top of the network pages: a round icon, what is going on, a refresh
/// button and the on/off switch. Returns (refresh pressed, switch pressed).
#[allow(clippy::too_many_arguments)]
pub fn status_tile(
    ui: &mut Ui,
    r: Rect,
    icon: Icon,
    tint: Color,
    text: &str,
    text_color: Color,
    on: Option<bool>,
    spinning: bool,
) -> (bool, bool) {
    tile(ui, r, r.h * 0.5, false);
    let d = r.h - 14.0;
    let c = Rect::new(r.x + 7.0, r.y + 7.0, d, d);
    ui.p.icon(icon, c.inset(d * 0.2), tint);
    let mut right = r.right() - 12.0;
    let mut toggled = false;
    if let Some(v) = on {
        let sw = Rect::new(right - 44.0, r.y, 44.0, r.h);
        toggled = crate::modules::widgets::switch(ui, sw, v, tint);
        right = sw.x - 8.0;
    }
    if spinning {
        ui.animate_slow();
    }
    let rb = Rect::new(right - 30.0, r.cy() - 15.0, 30.0, 30.0);
    let refresh = round_button(ui, rb, Icon::Refresh, false);
    ui.p.text(
        text,
        Rect::new(c.right() + 12.0, r.y, (rb.x - c.right() - 20.0).max(0.0), r.h),
        &TextStyle::new(15.0, text_color).semibold(),
    );
    (refresh, toggled)
}

// ---------------------------------------------------------------------------
// Notices (peeks)
// ---------------------------------------------------------------------------

/// Notices that carry a message (a copied text, a notification) are taller: they get a
/// second row with the text in its own soft box.
pub fn peek_tall(pk: &crate::modules::Peek) -> bool {
    if pk.is_hud() || pk.is_compact() || pk.scan {
        return false;
    }
    pk.key == Some("clipboard") || (pk.key.is_none() && pk.subtitle.chars().count() > 34)
}

/// A new notification: the sender's avatar, their name in bold and the message under it.
pub fn peek_notify(pk: &crate::modules::Peek) -> bool {
    pk.key == Some("notify") && !pk.is_hud() && !pk.scan
}

/// A device or network connecting or leaving: a small caption and the name under it.
pub fn peek_conn(pk: &crate::modules::Peek) -> bool {
    matches!(pk.key, Some("bluetooth" | "wifi" | "devices")) && !pk.subtitle.is_empty() && !pk.is_hud() && !pk.scan
}

/// A small rounded label.
fn tag(ui: &mut Ui, right: f32, cy: f32, text: &str, color: Color) -> f32 {
    // monochrome: a plain frosted label, white text, no coloured fill
    let mono = crate::ui::mono_icons();
    let (fill, color) = if mono { (Color::white(0.12), Color::white(0.92)) } else { (color.with_a(0.18), color) };
    let st = TextStyle::new(13.0, color).semibold();
    let (w, _) = ui.p.measure(text, &st);
    let r = Rect::new(right - w - 24.0, cy - 14.0, w + 24.0, 28.0);
    ui.p.fill_rounded(r, 14.0, fill);
    ui.p.text(text, Rect::new(r.x + 12.0, r.y, w + 6.0, r.h), &st);
    r.x
}

/// What a notice looks like: a coloured rounded tile with the icon, the title and a line
/// under it, and on the right whatever belongs to it (battery, a tag, a ring). Notices with
/// a message put it in a soft box below.
pub fn peek_content(ui: &mut Ui, inner: Rect, pk: &crate::modules::Peek) {
    use crate::modules::Trailing;
    if peek_notify(pk) {
        return notify_content(ui, inner, pk);
    }
    if peek_conn(pk) {
        return conn_content(ui, inner, pk);
    }
    let tall = peek_tall(pk);
    let ts = 46.0;
    let top_cy = if tall { inner.y + ts * 0.5 + 2.0 } else { inner.cy() };
    let tile_r = Rect::centered(inner.x + ts * 0.5, top_cy, ts, ts);
    match &pk.image {
        Some(img) => ui.p.image(img, tile_r, 15.0, 1.0),
        None => {
            // just the icon on the plain black (on a little glass disc in the monochrome look)
            icon_chip(ui, tile_r.inset(1.0), 1);
            ui.p.icon(pk.icon, tile_r.inset(5.0), pk.accent.lerp(Color::white(1.0), 0.15));
        }
    }

    // right side
    let mut right = inner.right();
    match &pk.trailing {
        Trailing::Battery { level, charging } => {
            let t = format!("{:.0}%", level * 100.0);
            let st = TextStyle::new(17.0, Color::white(1.0)).semibold().right();
            let (w, _) = ui.p.measure(&t, &st);
            ui.p.text(&t, Rect::new(right - w - 4.0, top_cy - 14.0, w + 4.0, 28.0), &st);
            right -= w + 14.0;
            battery_glyph(ui, Rect::new(right - 38.0, top_cy - 9.0, 38.0, 18.0), *level, *charging);
            right -= 38.0 + 10.0;
        }
        Trailing::Text(s, c) => right = tag(ui, right, top_cy, s, *c) - 10.0,
        Trailing::Ring(f, c) => {
            ui.ring(right - 17.0, top_cy, 34.0, 4.0, *f, *c);
            right -= 44.0;
        }
        _ => {}
    }
    if pk.key == Some("devices") && !pk.subtitle.is_empty() {
        right = tag(ui, right, top_cy, &pk.subtitle, pk.accent) - 10.0;
    }

    let tx = tile_r.right() + 14.0;
    let tw = (right - tx).max(20.0);
    let title_st = TextStyle::new(16.0, Color::white(1.0)).semibold();
    let sub_st = TextStyle::new(13.0, palette::TEXT_DIM);
    let sub_here = !tall && !pk.subtitle.is_empty() && pk.key != Some("devices");
    if sub_here {
        ui.p.text(&pk.title, Rect::new(tx, top_cy - 22.0, tw, 22.0), &title_st);
        ui.p.text(&pk.subtitle, Rect::new(tx, top_cy + 1.0, tw, 20.0), &sub_st);
    } else {
        ui.p.text(&pk.title, Rect::new(tx, top_cy - 12.0, tw, 24.0), &title_st);
    }
    if tall && !pk.subtitle.is_empty() {
        let b =
            Rect::new(inner.x, tile_r.bottom() + 10.0, inner.w, (inner.bottom() - tile_r.bottom() - 10.0).max(24.0));
        ui.p.push_clip(b);
        ui.p.text(
            &pk.subtitle,
            Rect::new(b.x + 2.0, b.y + 4.0, b.w - 4.0, b.h - 8.0),
            &TextStyle::new(13.5, Color::white(0.88)).wrap().top(),
        );
        ui.p.pop_clip();
    }
}

fn notify_content(ui: &mut Ui, inner: Rect, pk: &crate::modules::Peek) {
    use crate::modules::Trailing;
    // avatar: a round tile with the sender's first letter and a small app badge
    let d = 46.0;
    let av = Rect::new(inner.x, inner.cy() - d * 0.5, d, d);
    ui.p.fill_circle(av.cx(), av.cy(), d * 0.5, Color::white(0.13));
    let letter: String = pk.title.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
    ui.p.text(&letter, av, &TextStyle::new(20.0, Color::white(0.95)).semibold().center());
    let (bx, by) = (av.right() - 5.0, av.bottom() - 5.0);
    ui.p.fill_circle(bx, by, 10.5, Color::white(0.95));
    ui.p.icon(pk.icon, Rect::centered(bx, by, 13.0, 13.0), palette::INK);

    // the unread count as a red badge on the right
    let mut right = inner.right();
    if let Trailing::Text(count, color) = &pk.trailing {
        let c = Rect::centered(right - 12.0, inner.cy() - 14.0, 24.0, 24.0);
        ui.p.fill_circle(c.cx(), c.cy(), 12.0, *color);
        ui.p.text(count, c, &TextStyle::new(13.0, Color::white(1.0)).semibold().center());
        right -= 34.0;
    }

    let tx = av.right() + 14.0;
    let tw = (right - tx).max(20.0);
    let top = inner.cy() - 30.0;
    ui.p.push_clip(Rect::new(tx, top, tw, 62.0));
    ui.p.text(&pk.title, Rect::new(tx, top, tw, 24.0), &TextStyle::new(16.0, Color::white(1.0)).semibold());
    ui.p.text(&pk.subtitle, Rect::new(tx, top + 25.0, tw, 38.0), &TextStyle::new(13.5, palette::TEXT_DIM).wrap().top());
    ui.p.pop_clip();
}

fn conn_content(ui: &mut Ui, inner: Rect, pk: &crate::modules::Peek) {
    use crate::modules::Trailing;
    // a plain icon, a small caption and the name in bold
    let cy = inner.cy();
    let ic = Rect::centered(inner.x + 14.0, cy, 26.0, 26.0);
    ui.p.icon(pk.icon, ic, Color::white(0.92));
    let (caption, name) = if pk.key == Some("devices") {
        (pk.title.clone(), pk.subtitle.clone())
    } else {
        (pk.subtitle.split(" · ").next().unwrap_or("").to_string(), pk.title.clone())
    };
    let mut right = inner.right();
    if let Trailing::Battery { level, charging } = &pk.trailing {
        let t = format!("{:.0}%", level * 100.0);
        let st = TextStyle::new(15.0, Color::white(1.0)).semibold().right();
        let (w, _) = ui.p.measure(&t, &st);
        ui.p.text(&t, Rect::new(right - w - 4.0, cy - 12.0, w + 4.0, 24.0), &st);
        right -= w + 12.0;
        battery_glyph(ui, Rect::new(right - 34.0, cy - 8.0, 34.0, 16.0), *level, *charging);
        right -= 44.0;
    }
    let tx = ic.right() + 14.0;
    let tw = (right - tx).max(20.0);
    ui.p.push_clip(Rect::new(tx, cy - 24.0, tw, 48.0));
    ui.p.text(&caption, Rect::new(tx, cy - 21.0, tw, 18.0), &TextStyle::new(12.0, palette::TEXT_DIM).medium());
    ui.p.text(&name, Rect::new(tx, cy - 3.0, tw, 24.0), &TextStyle::new(16.0, Color::white(1.0)).semibold());
    ui.p.pop_clip();
}

// ---------------------------------------------------------------------------
// Building blocks for the Alcove pages
// ---------------------------------------------------------------------------

thread_local! {
    static SCROLL: RefCell<HashMap<u64, f32>> = RefCell::new(HashMap::new());
}

/// A soft rounded tile.
pub fn tile(ui: &mut Ui, r: Rect, radius: f32, hover: bool) {
    let h = ui.glide(crate::ui::rect_key(r, "alcove-tile"), hover as u8 as f32);
    ui.p.fill_rounded(r, radius, Color::white(0.07 + 0.05 * h));
}

/// How far a page title (icon, name and note) reaches, so chips can start after it.
pub fn title_w(ui: &mut Ui, r: Rect, name: &str) -> f32 {
    let d = r.h.min(30.0);
    let (w, _) = ui.p.measure(name, &TextStyle::new(16.0, Color::white(1.0)).semibold());
    d + 10.0 + w + 18.0
}

/// The soft badge behind a page's icon, the same on every page: a quiet fill, a hairline ring and
/// two faint white highlights on the top-right and bottom-left corners. It brightens a little under
/// the pointer. Monochrome look only.
pub fn icon_chip(ui: &mut Ui, c: Rect, key: usize) {
    if !crate::ui::mono_icons() {
        return;
    }
    let h = ui.glide(gkey("alcove-chip-glint", key), if ui.hovered(c) { 1.0 } else { 0.0 });
    if h > 0.01 && h < 0.99 {
        ui.animate();
    }
    chip_body(ui, c, c.w.min(c.h) * 0.5, h, 1.0);
}

/// The badge itself: fill, hairline ring and the two corner highlights, all scaled by `k` (0 - 1).
/// `h` is the pointer glow. The highlights are arcs of the corner circles, so they follow a round
/// badge, a rounded square or the end of a capsule alike, and come from the realization cache.
fn chip_body(ui: &mut Ui, c: Rect, radius: f32, h: f32, k: f32) {
    let radius = radius.min(c.w * 0.5).min(c.h * 0.5);
    ui.p.fill_rounded(c, radius, Color::white((0.07 + 0.05 * h) * k));
    ui.p.stroke_rounded(c.inset(0.5), radius - 0.5, 1.0, Color::white((0.045 + 0.05 * h) * k));
    let peak = (0.11 + 0.06 * h) * k;
    if peak <= 0.004 {
        return;
    }
    // each highlight is a few arcs laid over each other, shorter and shorter, so the light is
    // brightest in the middle and fades out quickly toward both ends
    let sweeps: [f32; 4] = [1.2, 0.9, 0.6, 0.3];
    let r = (radius - 0.5).max(1.0);
    let quarter = std::f32::consts::FRAC_PI_4;
    let corners = [(c.right() - radius, c.y + radius, quarter), (c.x + radius, c.bottom() - radius, quarter + std::f32::consts::PI)];
    for (cx, cy, mid) in corners {
        for sw in sweeps {
            ui.p.arc_cached(cx, cy, r, mid - sw * 0.5, sw, 1.3, Color::white(peak));
        }
    }
}

/// A Wi-Fi / Bluetooth style tab. The chosen one is a soft fill with white text (no ring or
/// highlights); the others are plain, dim text on the notch itself. Without monochrome icons it is the
/// ordinary white-when-chosen `chip`.
pub fn tab_chip(ui: &mut Ui, r: Rect, label: &str, icon: Option<Icon>, on: bool) -> bool {
    if !crate::ui::mono_icons() {
        return chip(ui, r, label, icon, on);
    }
    let (h, p) = ui.touch(r, "alcove-tab-touch");
    let a = ui.glide(crate::ui::rect_key(r, "alcove-tab"), if on { 1.0 } else { 0.0 });
    let rr = r.inset(1.2 * p);
    if a < 0.99 {
        ui.p.fill_rounded(rr, rr.h * 0.5, Color::white(0.05 * h * (1.0 - a)));
    }
    if a > 0.01 {
        ui.p.fill_rounded(rr, rr.h * 0.5, Color::white((0.10 + 0.04 * h) * a));
    }
    ui.glow(rr, rr.h * 0.5);
    let fg = Color::white(0.52 + 0.14 * h * (1.0 - a)).lerp(Color::white(0.97), a);
    let st = TextStyle::new(12.5, fg).semibold();
    let mut x = r.x + 13.0;
    if let Some(i) = icon {
        ui.p.icon(i, Rect::new(x, r.cy() - 7.0, 14.0, 14.0), fg);
        x += 20.0;
    } else {
        let (tw, _) = ui.p.measure(label, &st);
        x = r.x + ((r.w - tw) * 0.5).max(6.0);
    }
    ui.p.text(label, Rect::new(x, r.y, (r.right() - x).max(0.0), r.h), &st);
    ui.clicked(r)
}

/// A page title: a small round icon, the name and a dim note beside it.
pub fn title(ui: &mut Ui, r: Rect, icon: Icon, tint: Color, name: &str, note: &str) {
    let d = r.h.min(30.0);
    let c = Rect::new(r.x, r.cy() - d * 0.5, d, d);
    icon_chip(ui, c, 0);
    ui.p.icon(icon, c.inset(d * 0.2), tint);
    let st = TextStyle::new(16.0, Color::white(1.0)).semibold();
    let (w, _) = ui.p.measure(name, &st);
    ui.p.text(name, Rect::new(c.right() + 10.0, r.y, w + 4.0, r.h), &st);
    if !note.is_empty() {
        ui.p.text(
            note,
            Rect::new(c.right() + 10.0 + w + 10.0, r.y, 220.0, r.h),
            &TextStyle::new(13.0, palette::TEXT_FAINT).medium(),
        );
    }
}

/// How wide a chip with this label is.
pub fn chip_w(ui: &mut Ui, label: &str, icon: Option<Icon>) -> f32 {
    let (w, _) = ui.p.measure(label, &TextStyle::new(12.5, Color::white(1.0)).semibold());
    w + 26.0 + if icon.is_some() { 20.0 } else { 0.0 }
}

/// A capsule button. `on` makes it white (the chosen one).
pub fn chip(ui: &mut Ui, r: Rect, label: &str, icon: Option<Icon>, on: bool) -> bool {
    let (h, p) = ui.touch(r, "alcove-chip-touch");
    // the white fill eases in and out when the chosen chip changes
    let a = ui.glide(crate::ui::rect_key(r, "alcove-chip"), if on { 1.0 } else { 0.0 });
    let idle = Color::white(0.10 + 0.07 * h);
    let bg = idle.lerp(Color::white(0.94), a);
    // a press squeezes the chip a touch
    let rr = r.inset(1.2 * p);
    ui.p.fill_rounded(rr, rr.h * 0.5, bg);
    ui.glow(rr, rr.h * 0.5);
    let fg = Color::white(0.92).lerp(palette::INK, a);
    let st = TextStyle::new(12.5, fg).semibold();
    let mut x = r.x + 13.0;
    if let Some(i) = icon {
        ui.p.icon(i, Rect::new(x, r.cy() - 7.0, 14.0, 14.0), fg);
        x += 20.0;
    } else {
        // no icon: the label sits in the middle
        let (tw, _) = ui.p.measure(label, &st);
        x = r.x + ((r.w - tw) * 0.5).max(6.0);
    }
    ui.p.text(label, Rect::new(x, r.y, (r.right() - x).max(0.0), r.h), &st);
    ui.clicked(r)
}

/// A round icon button.
pub fn round_button(ui: &mut Ui, r: Rect, icon: Icon, filled: bool) -> bool {
    let (h, p) = ui.touch(r, "alcove-round");
    let bg = if filled { Color::white(0.94 + 0.06 * h) } else { Color::white(0.10 + 0.08 * h) };
    let d = r.w.min(r.h) * (1.0 + 0.03 * h - 0.07 * p);
    ui.p.fill_circle(r.cx(), r.cy(), d * 0.5, bg);
    ui.glow(r.center_square(d), d * 0.5);
    let fg = if filled { palette::INK } else { Color::white(0.92) };
    ui.p.icon(icon, r.center_square(d * 0.5), fg);
    ui.clicked(r)
}

/// A thin capsule gauge.
pub fn meter(ui: &mut Ui, r: Rect, frac: f32, color: Color) {
    ui.p.fill_rounded(r, r.h * 0.5, Color::white(0.14));
    let f = frac.clamp(0.0, 1.0);
    if f > 0.0 {
        ui.p.fill_rounded(Rect::new(r.x, r.y, (r.w * f).max(r.h), r.h), r.h * 0.5, color);
    }
}

/// Sideways scrolling for a row of cards: the wheel over `area` moves it (only when there is
/// something to scroll, so the wheel otherwise still turns the page). Returns the offset.
pub fn scroll_x(ui: &mut Ui, area: Rect, content_w: f32, tag: &str) -> f32 {
    let key = gkey(tag, 0);
    let max = (content_w - area.w).max(0.0);
    let mut target = SCROLL.with(|s| s.borrow().get(&key).copied().unwrap_or(0.0)).clamp(0.0, max);
    if max > 0.0 {
        let wheel = ui.wheel(area);
        if wheel != 0.0 {
            target = (target - wheel * 100.0).clamp(0.0, max);
        }
    }
    SCROLL.with(|s| s.borrow_mut().insert(key, target));
    let shown = ui.glide(key, target);
    if (shown - target).abs() > 0.3 {
        ui.animate();
    }
    shown
}

/// A calm empty state: an icon floating gently on a softly breathing halo, and one line below.
pub fn empty(ui: &mut Ui, r: Rect, icon: Icon, msg: &str) {
    let t = crate::ui::epoch_secs() * 1.4;
    let (bob, breath) = (t.sin() * 2.2, 0.5 + 0.5 * t.sin());
    let (cx, cy) = (r.cx(), r.cy() - 18.0);
    ui.p.fill_circle(cx, cy + bob * 0.4, 25.0 + 2.5 * breath, Color::white(0.035 + 0.02 * breath));
    ui.p.fill_circle(cx, cy + bob * 0.4, 19.0, Color::white(0.07));
    ui.p.icon(icon, Rect::centered(cx, cy + bob, 22.0, 22.0), Color::white(0.8));
    ui.p.text(
        msg,
        Rect::new(r.x + 12.0, r.cy() + 12.0, r.w - 24.0, 22.0),
        &TextStyle::new(13.0, Color::white(0.86)).center(),
    );
    // only keeps frames coming while the pointer is on the page, so an open notch left alone
    // settles and costs nothing
    if ui.hovered(r) {
        ui.animate_slow();
    }
}

thread_local! {
    static KICK: RefCell<HashMap<u64, (f32, Instant)>> = RefCell::new(HashMap::new());
}

/// Wheel scrolling for a list of rows: one row per notch, gliding into place so a row is never
/// left half cut. Pushing past either end gives a small elastic tug. `off` holds the target
/// (a whole number of rows); the returned value is the offset to draw with.
pub fn scroll_rows(ui: &mut Ui, area: Rect, off: &mut f32, content_h: f32, pitch: f32, tag: &str) -> f32 {
    let max = (content_h - area.h).max(0.0);
    *off = off.clamp(0.0, max);
    let (key, acc_key) = (gkey(tag, 0), gkey(tag, 1));
    let wheel = ui.wheel(area);
    if wheel != 0.0 {
        let mut acc = SCROLL.with(|s| s.borrow().get(&acc_key).copied().unwrap_or(0.0)) + wheel;
        if acc.abs() >= 1.0 {
            let step = if acc > 0.0 { -pitch } else { pitch };
            let next = (((*off / pitch).round() * pitch) + step).clamp(0.0, max);
            if (next - *off).abs() < 0.5 && max > 0.0 {
                let tug = if step > 0.0 { 12.0 } else { -12.0 };
                KICK.with(|k| k.borrow_mut().insert(key, (tug, ui.now)));
            }
            *off = next;
            acc = 0.0;
        }
        SCROLL.with(|s| s.borrow_mut().insert(acc_key, acc));
    }
    let shown = ui.glide(key, *off);
    let tug = KICK.with(|k| {
        k.borrow()
            .get(&key)
            .filter(|(_, at)| ui.now.saturating_duration_since(*at) < Duration::from_millis(150))
            .map_or(0.0, |(v, _)| *v)
    });
    let bump = ui.glide(gkey(tag, 3), tug);
    if tug != 0.0 || bump.abs() > 0.3 {
        ui.animate();
    }
    shown + bump
}

/// How big the unlock greeting's stage is: wide enough for the name, tall enough for two lines.
pub fn hello_stage(name: &str) -> (f32, f32) {
    // the ring on the left, "Hello" and the name beside it: wide rather than tall, so nothing
    // sits in the soft blur along the bottom edge
    if name.is_empty() {
        (290.0, 150.0)
    } else {
        ((180.0 + name.chars().count() as f32 * 13.0).clamp(310.0, 470.0), 150.0)
    }
}
