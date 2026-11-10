//! The Bloom look's own notch screens: the header (system load, the clock with page arrows,
//! battery and weather), the scrollable tab strip and the control center that is the home page.
//!
//! Everything is drawn with the same primitives as the other looks; only the layout and the
//! shapes here are specific to Bloom.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::gfx::{icons, Icon, TextStyle};
use crate::modules::{Indicator, Module, ModuleId};
use crate::ui::{gkey, rect_key, Ui};
use crate::util::{palette, Color, Rect};

/// Height of the header row plus the tab strip.
pub const TOP_H: f32 = 92.0;
/// Space left and right of the content, and under it.
pub const SIDE: f32 = 22.0;
pub const BOTTOM: f32 = 20.0;
/// Height of the control center.
pub const HOME_H: f32 = 270.0;

const PILL_H: f32 = 58.0;
const PILL_GAP: f32 = 12.0;
const CIRCLE: f32 = 52.0;
const INK: Color = palette::INK;

static DND: AtomicBool = AtomicBool::new(false);

/// Do not disturb: transient banners stay away (volume and brightness still show).
pub fn dnd() -> bool {
    DND.load(Ordering::Relaxed)
}

thread_local! {
    /// The page whose pill was last scrolled into view.
    static LAST_PAGE: Cell<u64> = const { Cell::new(0) };
    /// Where the brightness slider is being dragged to (applied when the mouse is released).
    static BRIGHT_DRAG: Cell<Option<f32>> = const { Cell::new(None) };
    /// Last volume sent while dragging, so small jitters do not flood the audio thread.
    static VOL_SENT: Cell<f32> = const { Cell::new(-1.0) };
}

fn hash_id(id: &str) -> u64 {
    id.bytes().fold(1469598103934665603u64, |h, b| (h ^ b as u64).wrapping_mul(1099511628211))
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

/// Header row and tab strip. `full` is the whole expanded notch.
pub fn header(
    ui: &mut Ui,
    full: Rect,
    modules: &[Box<dyn Module>],
    tabs: &[(ModuleId, Icon, String)],
    page: ModuleId,
    indicators: &[Indicator],
) {
    let row = Rect::new(full.x + SIDE, full.y + 14.0, full.w - 2.0 * SIDE, 32.0);
    let cy = row.cy();
    let text = TextStyle::new(14.5, Color::white(0.97)).semibold();

    // left: memory and processor load
    let mut x = row.x;
    for (icon, pct) in
        [(Icon::Ram, crate::modules::stats::ram_percent()), (Icon::Cpu, crate::modules::stats::cpu_percent())]
    {
        let s = pct.map_or("--%".to_string(), |v| format!("{v}%"));
        ui.p.icon(icon, Rect::new(x, cy - 9.0, 18.0, 18.0), Color::white(0.92));
        let (w, _) = ui.p.measure(&s, &text);
        ui.p.text(&s, Rect::new(x + 25.0, row.y, w + 4.0, row.h), &text);
        x += 25.0 + w + 16.0;
    }
    let left_end = x;

    // right: weather and battery
    let mut rx = row.right();
    let unit = if ui.cfg.weather.units == "imperial" { "F" } else { "C" };
    if let Some(i) = modules.iter().position(|m| m.id() == "weather") {
        if let Some((_, t)) = modules[i].glance() {
            let s = format!("{t}{unit}");
            let (w, _) = ui.p.measure(&s, &text);
            ui.p.text(&s, Rect::new(rx - w - 2.0, row.y, w + 4.0, row.h), &text);
            rx -= w + 6.0;
            ui.p.icon(Icon::Cloud, Rect::new(rx - 20.0, cy - 10.0, 20.0, 20.0), Color::white(0.92));
            rx -= 20.0 + 16.0;
        }
    }
    for ind in indicators.iter().rev() {
        if let Some((level, charging)) = ind.battery {
            let s = format!("{:.0}%", level * 100.0);
            let (w, _) = ui.p.measure(&s, &text);
            ui.p.text(&s, Rect::new(rx - w - 2.0, row.y, w + 4.0, row.h), &text);
            rx -= w + 8.0;
            let br = Rect::new(rx - 28.0, cy - 7.0, 28.0, 14.0);
            icons::battery(ui.p, br, level, charging, crate::app::battery_color(level, charging));
            rx -= 28.0 + 16.0;
        }
    }
    let right_start = rx;

    // centre: the time between two arrows that step through the pages
    let (h, m, _s, suffix, _date) = crate::app::clock_parts();
    let time = if suffix.is_empty() { format!("{h:02}:{m:02}") } else { format!("{h:02}:{m:02} {suffix}") };
    let big = TextStyle::new(22.0, Color::white(1.0)).semibold();
    let (tw, _) = ui.p.measure(&time, &big);
    let cx =
        row.cx().clamp(left_end + tw * 0.5 + 40.0, (right_start - tw * 0.5 - 40.0).max(left_end + tw * 0.5 + 40.0));
    ui.p.text(&time, Rect::new(cx - tw * 0.5 - 2.0, row.y, tw + 4.0, row.h), &big);
    let cur = tabs.iter().position(|t| t.0 == page);
    for dir in [-1.0f32, 1.0] {
        let b = Rect::new(cx + dir * (tw * 0.5 + 24.0) - 14.0, cy - 14.0, 28.0, 28.0);
        let hover = ui.hovered(b);
        if hover {
            ui.hot = true;
        }
        let a = ui.glide(rect_key(b, "bloom-arrow"), if hover { 1.0 } else { 0.0 });
        let c = Color::white(0.55 + 0.4 * a);
        let k = 5.0;
        let px = b.cx();
        // the tip points the way the arrow goes
        ui.p.line(px - dir * k * 0.6, cy - k * 1.1, px + dir * k * 0.6, cy, 1.9, c);
        ui.p.line(px + dir * k * 0.6, cy, px - dir * k * 0.6, cy + k * 1.1, 1.9, c);
        if ui.clicked(b) && !tabs.is_empty() {
            let n = tabs.len();
            let next = match cur {
                Some(i) if dir > 0.0 => (i + 1) % n,
                Some(i) => (i + n - 1) % n,
                None => 0,
            };
            ui.fx.open_page = Some(tabs[next].0);
        }
    }

    tab_strip(ui, Rect::new(full.x + SIDE, full.y + 52.0, full.w - 2.0 * SIDE, 34.0), modules, tabs, page);
}

/// The scrollable strip of page pills. The wheel scrolls it; the open page is kept in view.
fn tab_strip(ui: &mut Ui, strip: Rect, modules: &[Box<dyn Module>], tabs: &[(ModuleId, Icon, String)], page: ModuleId) {
    let st = TextStyle::new(13.0, Color::white(0.8)).medium();
    let mut items: Vec<(ModuleId, Icon, String, f32)> = Vec::new();
    for (id, icon, _) in tabs {
        let title = if *id == "home" {
            "Home".to_string()
        } else {
            modules.iter().find(|m| m.id() == *id).map(|m| short_title(m.title())).unwrap_or_default()
        };
        let (w, _) = ui.p.measure(&title, &st);
        items.push((*id, *icon, title, 14.0 + 16.0 + 7.0 + w + 14.0));
    }
    let gap = 8.0;
    let total: f32 = items.iter().map(|i| i.3).sum::<f32>() + gap * items.len().saturating_sub(1) as f32;
    let max_scroll = (total - strip.w).max(0.0);

    let key = gkey("bloom-strip", 0);
    thread_local! {
        static TARGET: Cell<f32> = const { Cell::new(0.0) };
    }
    let mut target = TARGET.with(|t| t.get()).clamp(0.0, max_scroll);
    // the open page's pill is brought to the middle when the page changes
    let ph = hash_id(page);
    if LAST_PAGE.with(|l| l.replace(ph)) != ph {
        let mut x = 0.0;
        for it in &items {
            if it.0 == page {
                target = (x + it.3 * 0.5 - strip.w * 0.5).clamp(0.0, max_scroll);
            }
            x += it.3 + gap;
        }
    }
    let wheel = ui.wheel(strip);
    if wheel != 0.0 {
        target = (target - wheel.signum() * 96.0).clamp(0.0, max_scroll);
    }
    TARGET.with(|t| t.set(target));
    let scroll = ui.glide(key, target);
    if (scroll - target).abs() > 0.3 {
        ui.animate();
    }

    ui.p.push_clip(strip);
    let mut x = strip.x - scroll;
    for (id, icon, title, w) in &items {
        let r = Rect::new(x, strip.y + 2.0, *w, strip.h - 4.0);
        x += w + gap;
        if r.right() < strip.x - 4.0 || r.x > strip.right() + 4.0 {
            continue;
        }
        let sel = *id == page;
        let hover = ui.hovered(r) && ui.hovered(strip);
        if hover {
            ui.hot = true;
        }
        let on = ui.glide(gkey(id, 71), if sel { 1.0 } else { 0.0 });
        let (sel_bg, sel_rim, sel_fg) = chosen();
        let bg = Color::white(if hover { 0.11 } else { 0.06 }).lerp(sel_bg, on);
        ui.p.fill_rounded(r, r.h * 0.5, bg);
        if on < 0.99 {
            ui.p.stroke_rounded(r.inset(0.5), r.h * 0.5, 1.0, Color::white(0.09 * (1.0 - on)));
        }
        if sel_rim.a > 0.0 && on > 0.01 {
            ui.p.stroke_rounded(r.inset(0.5), r.h * 0.5 - 0.5, 1.0, sel_rim.with_a(sel_rim.a * on));
        }
        let fg = Color::white(if hover { 0.95 } else { 0.78 }).lerp(sel_fg, on);
        ui.p.icon(*icon, Rect::new(r.x + 14.0, r.cy() - 8.0, 16.0, 16.0), fg);
        ui.p.text(title, Rect::new(r.x + 14.0 + 16.0 + 7.0, r.y, r.w, r.h), &st.color(fg));
        if !sel && ui.clicked(r) && ui.hovered(strip) {
            ui.fx.open_page = Some(*id);
        }
    }
    ui.p.pop_clip();

    // soft edges where more pills are hidden
    let fade = 26.0;
    let black = Color::black(1.0);
    if scroll > 1.0 {
        let pts = [
            (strip.x, strip.y),
            (strip.x + fade, strip.y),
            (strip.x + fade, strip.bottom()),
            (strip.x, strip.bottom()),
        ];
        ui.p.fill_gradient_poly(&pts, &[(0.0, black), (1.0, black.with_a(0.0))], strip.x, strip.x + fade);
    }
    if scroll < max_scroll - 1.0 {
        let x0 = strip.right() - fade;
        let pts = [(x0, strip.y), (strip.right(), strip.y), (strip.right(), strip.bottom()), (x0, strip.bottom())];
        ui.p.fill_gradient_poly(&pts, &[(0.0, black.with_a(0.0)), (1.0, black)], x0, strip.right());
    }
}

/// Page names can be long ("Wi-Fi & Bluetooth"): the strip uses short ones.
fn short_title(t: &str) -> String {
    match t {
        "Wi-Fi & Bluetooth" => "Network".to_string(),
        "Volume & Brightness" => "Sound".to_string(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Control center (home page)
// ---------------------------------------------------------------------------

pub fn control_center(ui: &mut Ui, r: Rect, modules: &mut [Box<dyn Module>], started: &[bool]) {
    let cc = modules.iter().position(|m| m.id() == "control").filter(|&i| started[i]);
    let wifi = cc.and_then(|i| modules[i].glance_for("wifi")).map(|g| g.1);
    let bt = cc.and_then(|i| modules[i].glance_for("bluetooth")).map(|g| g.1);
    let wifi_on = wifi.as_deref().is_some_and(|s| s != "Off");
    let bt_on = bt.as_deref().is_some_and(|s| s != "Off");

    let pw = (r.w - PILL_GAP) * 0.5;
    let cell = |col: usize, row: usize| {
        Rect::new(r.x + col as f32 * (pw + PILL_GAP), r.y + row as f32 * (PILL_H + PILL_GAP), pw, PILL_H)
    };

    // Wi-Fi and Bluetooth open their lists
    if pill(ui, cell(0, 0), Icon::Wifi, "Wi-Fi", wifi.as_deref().unwrap_or("Not available"), wifi_on, true)
        && cc.is_some()
    {
        crate::modules::control::WANT_TAB.store(0, Ordering::Relaxed);
        ui.fx.open_page = Some("control");
    }
    if pill(ui, cell(0, 1), Icon::Bluetooth, "Bluetooth", bt.as_deref().unwrap_or("Not available"), bt_on, true)
        && cc.is_some()
    {
        crate::modules::control::WANT_TAB.store(1, Ordering::Relaxed);
        ui.fx.open_page = Some("control");
    }

    // Glass edge: on or off
    let glass_on = ui.cfg.appearance.glass_look;
    if pill(ui, cell(1, 0), Icon::Control, "Glass Edge", if glass_on { "On" } else { "Off" }, false, false) {
        ui.fx.save_config.push(("appearance".into(), "glass_look".into(), (!glass_on).to_string()));
    }

    // Notch Mode: always shown, a thin pill when idle, or hidden until something happens
    let idle = ui.cfg.general.idle_style.clone();
    let (idle_text, idle_next) = match idle.as_str() {
        "pill" => ("Smart", "hidden"),
        "hidden" => ("Hidden", "notch"),
        _ => ("Always", "pill"),
    };
    if pill(ui, cell(1, 1), Icon::Home, "Notch Mode", idle_text, false, false) {
        ui.fx.save_config.push(("general".into(), "idle_style".into(), format!("\"{idle_next}\"")));
    }

    // round actions
    let top = r.y + 2.0 * PILL_H + PILL_GAP + 18.0;
    let acts: [(Icon, &str, bool); 5] = [
        (Icon::Moon, "Do not disturb", dnd()),
        (Icon::Grid, "shelf", false),
        (Icon::Bell, "notifications", false),
        (Icon::Settings, "settings", false),
        (Icon::Refresh, "reload", false),
    ];
    let span = r.w - CIRCLE;
    for (i, (icon, what, on)) in acts.iter().enumerate() {
        let c = Rect::new(r.x + span * i as f32 / 4.0, top, CIRCLE, CIRCLE);
        if circle(ui, c, *icon, *on) {
            match *what {
                "Do not disturb" => DND.store(!dnd(), Ordering::Relaxed),
                "reload" => ui.fx.reload_config = true,
                page => {
                    ui.fx.open_page = Some(match page {
                        "shelf" => "shelf",
                        "notifications" => "notifications",
                        _ => "settings",
                    })
                }
            }
        }
    }

    // sliders
    let sy = top + CIRCLE + 22.0;
    let vol = crate::modules::osd::volume_now();
    let vol_icon = if vol == Some(0.0) { Icon::Mute } else { Icon::Volume };
    let shown = slider_row(ui, Rect::new(r.x, sy, r.w, 30.0), vol_icon, "Volume", vol);
    if let Some(v) = shown {
        if (VOL_SENT.with(|s| s.get()) - v).abs() > 0.004 {
            VOL_SENT.with(|s| s.set(v));
            ui.fx.set_volume = Some(v);
        }
    }
    let bright = crate::modules::osd::brightness_now();
    let drag = slider_row(ui, Rect::new(r.x, sy + 38.0, r.w, 30.0), Icon::Brightness, "Brightness", bright);
    match (drag, BRIGHT_DRAG.with(|b| b.get())) {
        (Some(v), _) => BRIGHT_DRAG.with(|b| b.set(Some(v))),
        (None, Some(v)) => {
            // released: apply once
            BRIGHT_DRAG.with(|b| b.set(None));
            ui.fx.set_brightness = Some((v * 100.0).round() as u8);
        }
        (None, None) => {}
    }
}

/// What a chosen control is filled with, the colour of its edge and the colour of what is written on it: white
/// with dark ink, or in Tuna the launcher's deep blue with white.
fn chosen() -> (Color, Color, Color) {
    if crate::ui::look_tuna() {
        (crate::ui::tuna_sel(), crate::ui::tuna_rim(), Color::white(1.0))
    } else {
        (Color::white(0.96), Color::white(0.0), INK)
    }
}

/// A rounded control with an icon, a title and a status line. `on` fills it white.
fn pill(ui: &mut Ui, r: Rect, icon: Icon, title: &str, status: &str, on: bool, chevron: bool) -> bool {
    let hover = ui.hovered(r);
    if hover {
        ui.hot = true;
    }
    let t = ui.glide(rect_key(r, "bloom-pill"), if on { 1.0 } else { 0.0 });
    let h = ui.glide(rect_key(r, "bloom-pill-h"), if hover { 1.0 } else { 0.0 });
    let (sel_bg, sel_rim, sel_fg) = chosen();
    let tuna = crate::ui::look_tuna();
    let bg = Color::white(0.035 + 0.045 * h).lerp(if tuna { sel_bg } else { Color::white(0.97 - 0.05 * h) }, t);
    ui.p.fill_rounded(r, 18.0, bg);
    if t < 0.99 {
        ui.p.stroke_rounded(r.inset(0.5), 18.0, 1.0, Color::white(0.1 * (1.0 - t)));
    }
    if sel_rim.a > 0.0 && t > 0.01 {
        ui.p.stroke_rounded(r.inset(0.5), 17.5, 1.0, sel_rim.with_a(sel_rim.a * t));
    }
    let fg = Color::white(0.97).lerp(sel_fg, t);
    let sub = Color::white(0.55).lerp(if tuna { Color::white(0.72) } else { Color::black(0.55) }, t);
    ui.p.icon(icon, Rect::new(r.x + 18.0, r.cy() - 10.0, 20.0, 20.0), fg);
    let tx = r.x + 52.0;
    let tw = (r.w - 52.0 - if chevron { 34.0 } else { 14.0 }).max(10.0);
    ui.p.text(title, Rect::new(tx, r.y + 9.0, tw, 22.0), &TextStyle::new(15.0, fg).semibold());
    ui.p.text(status, Rect::new(tx, r.y + 31.0, tw, 18.0), &TextStyle::new(12.0, sub));
    if chevron {
        let (cx, cy) = (r.right() - 20.0, r.cy());
        ui.p.line(cx - 2.5, cy - 5.0, cx + 2.5, cy, 1.8, sub);
        ui.p.line(cx + 2.5, cy, cx - 2.5, cy + 5.0, 1.8, sub);
    }
    ui.clicked(r)
}

/// A round action button.
fn circle(ui: &mut Ui, r: Rect, icon: Icon, on: bool) -> bool {
    let hover = ui.hovered(r);
    if hover {
        ui.hot = true;
    }
    let t = ui.glide(rect_key(r, "bloom-circle"), if on { 1.0 } else { 0.0 });
    let h = ui.glide(rect_key(r, "bloom-circle-h"), if hover { 1.0 } else { 0.0 });
    let (sel_bg, sel_rim, sel_fg) = chosen();
    let bg = Color::white(0.03 + 0.06 * h).lerp(sel_bg, t);
    ui.p.fill_circle(r.cx(), r.cy(), r.w * 0.5, bg);
    if t < 0.99 {
        ui.p.stroke_circle(r.cx(), r.cy(), r.w * 0.5 - 0.5, 1.0, Color::white(0.1 * (1.0 - t)));
    }
    if sel_rim.a > 0.0 && t > 0.01 {
        ui.p.stroke_circle(r.cx(), r.cy(), r.w * 0.5 - 0.5, 1.0, sel_rim.with_a(sel_rim.a * t));
    }
    let fg = Color::white(0.95).lerp(sel_fg, t);
    ui.p.icon(icon, r.center_square(22.0), fg);
    ui.clicked(r)
}

/// One slider row: icon and name, a capsule track and the percentage. Returns the value being
/// dragged to while the pointer is down on it.
fn slider_row(ui: &mut Ui, row: Rect, icon: Icon, name: &str, value: Option<f32>) -> Option<f32> {
    let label = TextStyle::new(14.0, Color::white(0.86)).medium();
    ui.p.icon(icon, Rect::new(row.x + 2.0, row.cy() - 9.0, 18.0, 18.0), Color::white(0.62));
    ui.p.text(name, Rect::new(row.x + 30.0, row.y, 80.0, row.h), &label);
    let track = Rect::new(row.x + 116.0, row.cy() - 5.0, (row.w - 116.0 - 56.0).max(40.0), 10.0);
    let hit = Rect::new(track.x - 8.0, track.y - 12.0, track.w + 16.0, track.h + 24.0);
    let pressed = ui.pressed(hit);
    let hover = ui.hovered(hit);
    if hover {
        ui.hot = true;
    }
    let mut dragging = None;
    if pressed {
        if let Some((mx, _)) = ui.input.mouse {
            dragging = Some(((mx - track.x) / track.w).clamp(0.0, 1.0));
        }
        ui.animate();
    }
    let known = value.is_some() || dragging.is_some();
    let base = value.unwrap_or(0.5);
    let key = rect_key(row, "bloom-slider");
    let shown = match dragging {
        Some(v) => v,
        None => ui.glide(key, base),
    };
    let grow = ui.glide(rect_key(row, "bloom-slider-h"), if hover || pressed { 1.0 } else { 0.0 });
    let t = 10.0 + 2.0 * grow;
    let tr = Rect::new(track.x, track.cy() - t * 0.5, track.w, t);
    ui.p.fill_rounded(tr, t * 0.5, Color::white(0.11));
    let fill = if !known {
        Color::white(0.25)
    } else if crate::ui::look_tuna() {
        crate::ui::bloom_blue()
    } else {
        Color::white(0.97)
    };
    ui.p.fill_rounded(Rect::new(tr.x, tr.y, (tr.w * shown).max(t.min(tr.w)), t), t * 0.5, fill);
    let pct = if known { format!("{:.0}%", shown * 100.0) } else { "--".to_string() };
    ui.p.text(
        &pct,
        Rect::new(row.right() - 52.0, row.y, 52.0, row.h),
        &TextStyle::new(14.0, Color::white(0.85)).medium().right(),
    );
    dragging
}
