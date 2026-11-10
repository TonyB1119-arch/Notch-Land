//! Weather: current conditions and a five day forecast from open-meteo.com
//! (free, no account, no key). The city comes from the config or from
//! Settings → Weather; it is looked up once with Open-Meteo's geocoder.
//!
//! Requests happen on a worker thread: at start, when the city or units
//! change, and when the notch opens while the data is older than
//! `refresh_minutes`. Nothing runs in the background otherwise.

use std::any::Any;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{CardSize, Cx, Module, ModuleId, SystemEvent};
use crate::bus::Bus;
use crate::gfx::icons::{self, Sky};
use crate::gfx::{Icon, Painter, TextStyle};
use crate::sys::http::{self, Timeouts};
use crate::ui::{ButtonStyle, Ui};
use crate::util::{palette, Color, Rect};

const ID: ModuleId = "weather";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cond {
    Clear,
    PartlyCloudy,
    Cloudy,
    Fog,
    Drizzle,
    Rain,
    Snow,
    Storm,
}

impl Cond {
    /// WMO weather interpretation codes, as used by Open-Meteo.
    fn from_code(code: u32) -> Cond {
        match code {
            0 => Cond::Clear,
            1 | 2 => Cond::PartlyCloudy,
            3 => Cond::Cloudy,
            45 | 48 => Cond::Fog,
            51..=57 => Cond::Drizzle,
            61..=67 | 80..=82 => Cond::Rain,
            71..=77 | 85 | 86 => Cond::Snow,
            95..=99 => Cond::Storm,
            _ => Cond::Cloudy,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Cond::Clear => "Clear",
            Cond::PartlyCloudy => "Partly cloudy",
            Cond::Cloudy => "Cloudy",
            Cond::Fog => "Fog",
            Cond::Drizzle => "Drizzle",
            Cond::Rain => "Rain",
            Cond::Snow => "Snow",
            Cond::Storm => "Thunderstorm",
        }
    }
}

#[derive(Clone, Debug)]
struct Day {
    label: String,
    cond: Cond,
    hi: f32,
    lo: f32,
    rain: Option<f32>,
}

/// One hour of the forecast.
#[derive(Clone, Debug)]
struct Hour {
    hour: u32,
    cond: Cond,
    night: bool,
    temp: f32,
}

#[derive(Clone, Debug)]
struct Forecast {
    place: String,
    temp: f32,
    feels: f32,
    humidity: f32,
    wind: f32,
    cond: Cond,
    night: bool,
    days: Vec<Day>,
    /// The next eight hours, starting with the current one.
    hours: Vec<Hour>,
    imperial: bool,
}

enum WMsg {
    Data(Forecast),
    Error(String),
}

enum Cmd {
    Fetch { city: String, imperial: bool },
    Quit,
}

/// The latest forecast in short form, for the Alcove home page.
#[derive(Clone)]
pub struct Brief {
    pub temp: f32,
    pub place: String,
    pub label: &'static str,
    pub imperial: bool,
    /// The coming days (the first is today).
    pub days: Vec<BriefDay>,
    cond: Cond,
    night: bool,
}

/// One day of the forecast, in short form.
#[derive(Clone)]
pub struct BriefDay {
    pub label: String,
    pub hi: f32,
    pub lo: f32,
    pub rain: Option<f32>,
    cond: Cond,
}

impl Brief {
    pub fn draw_icon(&self, p: &mut Painter, r: Rect) {
        draw_condition(p, self.cond, r, self.night);
    }

    /// The icon of day `i` of the forecast.
    pub fn day_icon(&self, i: usize, p: &mut Painter, r: Rect) {
        if let Some(d) = self.days.get(i) {
            draw_condition(p, d.cond, r, false);
        }
    }

    /// "°C" or "°F".
    pub fn unit(&self) -> &'static str {
        if self.imperial {
            "°F"
        } else {
            "°C"
        }
    }
}

thread_local! {
    static BRIEF: std::cell::RefCell<Option<std::rc::Rc<Brief>>> = const { std::cell::RefCell::new(None) };
}

/// What the weather is now, when a forecast has arrived.
pub fn brief() -> Option<std::rc::Rc<Brief>> {
    BRIEF.with(|b| b.borrow().clone())
}

pub struct Weather {
    tx: Option<Sender<Cmd>>,
    data: Option<std::sync::Arc<Forecast>>,
    error: Option<String>,
    loading: bool,
    last_fetch: Option<Instant>,
    /// What the data on screen was fetched for.
    key: (String, bool),
}

impl Weather {
    pub fn new() -> Self {
        Self { tx: None, data: None, error: None, loading: false, last_fetch: None, key: (String::new(), false) }
    }

    fn fetch(&mut self, cx_cfg: &crate::config::Config) {
        let city = cx_cfg.weather.city.trim().to_string();
        let imperial = cx_cfg.weather.units.eq_ignore_ascii_case("imperial");
        if city.is_empty() {
            self.data = None;
            BRIEF.with(|b| *b.borrow_mut() = None);
            self.error = None;
            self.loading = false;
            return;
        }
        if let Some(tx) = &self.tx {
            self.loading = true;
            self.error = None;
            self.last_fetch = Some(Instant::now());
            self.key = (city.clone(), imperial);
            let _ = tx.send(Cmd::Fetch { city, imperial });
        }
    }

    fn unit(&self) -> &'static str {
        if self.data.as_ref().is_some_and(|d| d.imperial) {
            "°F"
        } else {
            "°C"
        }
    }
}

impl Module for Weather {
    fn id(&self) -> ModuleId {
        ID
    }
    fn title(&self) -> &str {
        "Weather"
    }
    fn icon(&self) -> Icon {
        Icon::Cloud
    }

    fn start(&mut self, cx: &mut Cx) {
        let (tx, rx) = channel();
        let bus = cx.bus.clone();
        let _ = std::thread::Builder::new().name("weather".into()).spawn(move || worker(rx, bus));
        self.tx = Some(tx);
        self.fetch(cx.cfg);
    }

    fn stop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Cmd::Quit);
        }
    }

    fn on_message(&mut self, msg: Box<dyn Any + Send>, cx: &mut Cx) {
        let Ok(m) = msg.downcast::<WMsg>() else { return };
        self.loading = false;
        match *m {
            WMsg::Data(d) => {
                let days = d
                    .days
                    .iter()
                    .map(|x| BriefDay { label: x.label.clone(), hi: x.hi, lo: x.lo, rain: x.rain, cond: x.cond })
                    .collect();
                let brief = Brief {
                    temp: d.temp,
                    place: d.place.clone(),
                    label: d.cond.label(),
                    imperial: d.imperial,
                    days,
                    cond: d.cond,
                    night: d.night,
                };
                BRIEF.with(|b| *b.borrow_mut() = Some(std::rc::Rc::new(brief)));
                self.data = Some(std::sync::Arc::new(d));
                self.error = None;
            }
            WMsg::Error(e) => self.error = Some(e),
        }
        cx.fx.redraw = true;
        cx.fx.layout_changed = true;
    }

    fn on_system(&mut self, ev: &SystemEvent, cx: &mut Cx) {
        match ev {
            SystemEvent::Expanded(true) => {
                let max_age = Duration::from_secs(cx.cfg.weather.refresh_minutes as u64 * 60);
                let stale = self.last_fetch.is_none_or(|t| t.elapsed() > max_age);
                if stale && !self.loading {
                    self.fetch(cx.cfg);
                }
            }
            SystemEvent::ConfigReloaded => {
                let city = cx.cfg.weather.city.trim().to_string();
                let imperial = cx.cfg.weather.units.eq_ignore_ascii_case("imperial");
                if self.key != (city, imperial) {
                    self.fetch(cx.cfg);
                    cx.fx.layout_changed = true;
                }
            }
            _ => {}
        }
    }

    fn glance(&self) -> Option<(Icon, String)> {
        self.data.as_ref().map(|d| (Icon::Cloud, format!("{:.0}°", d.temp)))
    }

    fn card(&self) -> Option<CardSize> {
        Some(CardSize::Small)
    }

    fn draw_card(&mut self, ui: &mut Ui, r: Rect) {
        let open = ui.card(r, true);
        if open {
            ui.fx.open_page = Some(ID);
        }
        let inner = r.inset_xy(12.0, 0.0);
        let Some(d) = self.data.clone() else {
            let msg = if ui.cfg.weather.city.trim().is_empty() {
                "Set a city for weather"
            } else if self.error.is_some() {
                "Weather unavailable"
            } else {
                "Loading weather…"
            };
            ui.p.icon(Icon::Cloud, Rect::new(inner.x, r.cy() - 8.0, 16.0, 16.0), palette::TEXT_FAINT);
            ui.p.text(
                msg,
                Rect::new(inner.x + 24.0, r.y, inner.w - 24.0, r.h),
                &TextStyle::new(12.0, palette::TEXT_DIM),
            );
            return;
        };
        let isz = (r.h - 8.0).clamp(14.0, 26.0);
        draw_condition(ui.p, d.cond, Rect::new(inner.x, r.cy() - isz * 0.5, isz, isz), d.night);
        let tx = inner.x + isz + 8.0;
        let temp = format!("{:.0}°", d.temp);
        let (tw, _) = ui.p.measure(&temp, &TextStyle::new(14.0, palette::TEXT).semibold().display());
        ui.p.text(&temp, Rect::new(tx, r.y, tw + 4.0, r.h), &TextStyle::new(14.0, palette::TEXT).semibold().display());
        ui.p.text(
            &format!("{} · {}", d.cond.label(), d.place),
            Rect::new(tx + tw + 8.0, r.y, (inner.right() - tx - tw - 8.0).max(0.0), r.h),
            &TextStyle::new(11.5, palette::TEXT_DIM),
        );
    }

    fn has_page(&self) -> bool {
        true
    }

    fn page_height(&self, _w: f32) -> f32 {
        if crate::ui::look_alcove() {
            268.0
        } else {
            238.0
        }
    }

    fn draw_page(&mut self, ui: &mut Ui, r: Rect) {
        let city_set = !ui.cfg.weather.city.trim().is_empty();
        if !city_set {
            ui.empty_state(Rect::new(r.x, r.y, r.w, r.h - 40.0), Icon::Cloud, "Choose a city to see the weather");
            let b = Rect::new(r.cx() - 70.0, r.bottom() - 36.0, 140.0, 30.0);
            if ui.pill_button(b, "Open settings", Some(Icon::Settings), ButtonStyle::filled(Color::white(0.1))) {
                ui.fx.open_page = Some("settings");
            }
            return;
        }
        let Some(d) = self.data.clone() else {
            if let Some(e) = self.error.clone() {
                ui.empty_state(Rect::new(r.x, r.y, r.w, r.h - 40.0), Icon::Cloud, &e);
                let b = Rect::new(r.cx() - 50.0, r.bottom() - 36.0, 100.0, 30.0);
                if ui.pill_button(b, "Retry", Some(Icon::Refresh), ButtonStyle::filled(Color::white(0.1))) {
                    self.fetch(ui.cfg);
                }
            } else {
                ui.empty_state(r, Icon::Cloud, "Loading weather…");
                ui.ticks_sweep(Rect::new(r.cx() - 50.0, r.cy() + 36.0, 100.0, 10.0), Color::white(0.9));
            }
            return;
        };

        if crate::ui::look_alcove() {
            return self.draw_alcove(ui, r, &d);
        }

        // top: the weather now. Icon and temperature, the details beside them,
        // humidity and wind as beam gauges on the right.
        let top_h = 92.0;
        let now = Rect::new(r.x, r.y, r.w, top_h);
        let ix = now.x + 6.0;
        draw_condition(ui.p, d.cond, Rect::new(ix, now.y + 4.0, 64.0, 64.0), d.night);
        let temp = format!("{:.0}{}", d.temp, self.unit());
        let tstyle = TextStyle::new(50.0, palette::TEXT).semibold().display();
        let (tw, _) = ui.p.measure(&temp, &tstyle);
        ui.p.text(&temp, Rect::new(ix + 80.0, now.y - 2.0, tw + 8.0, 70.0), &tstyle);
        let dx = ix + 80.0 + tw + 28.0;
        ui.p.text(
            d.cond.label(),
            Rect::new(dx, now.y + 8.0, 190.0, 22.0),
            &TextStyle::new(16.0, palette::TEXT).medium(),
        );
        ui.p.text(&d.place, Rect::new(dx, now.y + 32.0, 190.0, 18.0), &TextStyle::new(12.5, palette::TEXT_DIM));
        ui.p.text(
            &format!("Feels like {:.0}°", d.feels),
            Rect::new(dx, now.y + 52.0, 190.0, 18.0),
            &TextStyle::new(12.5, palette::TEXT_FAINT),
        );
        let wind_unit = if d.imperial { "mph" } else { "km/h" };
        let wind_max = if d.imperial { 40.0 } else { 64.0 };
        let gauges = [
            ("Humidity", format!("{:.0}%", d.humidity), d.humidity / 100.0),
            ("Wind", format!("{:.0} {}", d.wind, wind_unit), d.wind / wind_max),
        ];
        let gx = (now.right() - 232.0).max(dx + 196.0);
        let gw = now.right() - 6.0 - gx;
        for (k, (name, val, frac)) in gauges.iter().enumerate() {
            let y = now.y + 14.0 + k as f32 * 30.0;
            ui.p.text(name, Rect::new(gx, y, gw, 14.0), &TextStyle::new(11.0, palette::TEXT_FAINT));
            ui.p.text(val, Rect::new(gx, y, gw, 14.0), &TextStyle::new(11.5, palette::TEXT).semibold().right());
            ui.ticks(Rect::new(gx, y + 17.0, gw, 8.0), *frac, Color::white(0.92));
        }
        // refresh sits in the top corner
        let rb = Rect::new(now.right() - 24.0, now.y - 8.0, 24.0, 24.0);
        if self.loading {
            ui.animate_slow();
        }
        if ui.icon_button(rb, Icon::Refresh, ButtonStyle::default().scale(0.6).fg(palette::TEXT_FAINT)) && !self.loading
        {
            self.fetch(ui.cfg);
        }

        // below: the five-day forecast, flat columns on one hairline
        ui.p.fill_rect(Rect::new(r.x + 4.0, now.bottom() + 6.0, r.w - 8.0, 1.0), Color::white(0.08));
        let fc = Rect::new(r.x, now.bottom() + 12.0, r.w, r.bottom() - now.bottom() - 12.0);
        let n = d.days.len().max(1) as f32;
        let cw = fc.w / n;
        let lo_all = d.days.iter().map(|x| x.lo).fold(f32::MAX, f32::min);
        let hi_all = d.days.iter().map(|x| x.hi).fold(f32::MIN, f32::max);
        let span = (hi_all - lo_all).max(1.0);
        for (i, day) in d.days.iter().enumerate() {
            let c = Rect::new(fc.x + i as f32 * cw, fc.y, cw, fc.h);
            if i > 0 {
                ui.p.fill_rect(Rect::new(c.x, c.y + 6.0, 1.0, (c.h - 12.0).max(0.0)), Color::white(0.06));
            }
            let inner = c.inset_xy(16.0, 0.0);
            ui.p.text(
                &day.label,
                Rect::new(inner.x, c.y, inner.w, 16.0),
                &TextStyle::new(12.0, palette::TEXT_DIM).semibold().center(),
            );
            draw_condition(ui.p, day.cond, Rect::centered(inner.cx(), c.y + 42.0, 30.0, 30.0), false);
            ui.tick_range(
                Rect::new(inner.x, c.y + 66.0, inner.w, 8.0),
                (day.lo - lo_all) / span,
                (day.hi - lo_all) / span,
                Color::white(0.92),
            );
            ui.p.text(
                &format!("{:.0}°", day.lo),
                Rect::new(inner.x, c.y + 78.0, inner.w, 16.0),
                &TextStyle::new(12.0, palette::TEXT_FAINT),
            );
            ui.p.text(
                &format!("{:.0}°", day.hi),
                Rect::new(inner.x, c.y + 78.0, inner.w, 16.0),
                &TextStyle::new(13.0, palette::TEXT).semibold().right(),
            );
            if let Some(p) = day.rain.filter(|p| *p >= 20.0) {
                ui.p.text(
                    &format!("{p:.0}% rain"),
                    Rect::new(inner.x, c.y + 98.0, inner.w, 16.0),
                    &TextStyle::new(11.0, palette::BLUE).semibold().center(),
                );
            }
        }
    }
}

impl Weather {
    /// Alcove: the place, the big temperature and the details on the left of the big icon, and the
    /// next eight hours as a quiet row of columns underneath.
    fn draw_alcove(&mut self, ui: &mut Ui, r: Rect, d: &Forecast) {
        let isz = 92.0;
        draw_condition(ui.p, d.cond, Rect::new(r.x + 6.0, r.y + 34.0, isz, isz), d.night);
        let tx = r.x + isz + 34.0;
        let tw = (r.right() - tx).max(60.0);
        ui.p.text(&d.place, Rect::new(tx, r.y, tw, 24.0), &TextStyle::new(18.0, palette::TEXT).semibold());
        ui.p.text(
            &format!("{:.0}°", d.temp),
            Rect::new(tx, r.y + 22.0, tw, 66.0),
            &TextStyle::new(58.0, palette::TEXT).weight(700),
        );
        ui.p.text(d.cond.label(), Rect::new(tx, r.y + 90.0, tw, 22.0), &TextStyle::new(17.0, palette::TEXT).semibold());
        let mut y = r.y + 116.0;
        if let Some(day) = d.days.first() {
            ui.p.text(
                &format!("H: {:.0}°  L: {:.0}°", day.hi, day.lo),
                Rect::new(tx, y, tw, 18.0),
                &TextStyle::new(12.5, palette::TEXT_DIM).medium(),
            );
            y += 19.0;
        }
        let wind_unit = if d.imperial { "mph" } else { "km/h" };
        ui.p.text(
            &format!("Feels: {:.0}°     Wind: {:.0} {wind_unit}", d.feels, d.wind),
            Rect::new(tx, y, tw, 18.0),
            &TextStyle::new(12.0, palette::TEXT_FAINT),
        );

        // the next hours
        if d.hours.is_empty() {
            return;
        }
        let top = r.y + 168.0;
        ui.p.text(
            "HOURLY FORECAST",
            Rect::new(r.x + 2.0, top, 220.0, 14.0),
            &TextStyle::new(11.0, palette::TEXT_FAINT).semibold(),
        );
        let h24 = crate::app::clock_parts().3.is_empty();
        let cw = r.w / d.hours.len() as f32;
        let y0 = top + 24.0;
        for (i, hr) in d.hours.iter().enumerate() {
            let c = Rect::new(r.x + i as f32 * cw, y0, cw, 76.0);
            let label = if h24 {
                format!("{:02}", hr.hour)
            } else {
                let h12 = if hr.hour % 12 == 0 { 12 } else { hr.hour % 12 };
                format!("{h12}{}", if hr.hour >= 12 { "PM" } else { "AM" })
            };
            ui.p.text(&label, Rect::new(c.x, c.y, c.w, 16.0), &TextStyle::new(12.0, palette::TEXT_DIM).medium().center());
            draw_condition(ui.p, hr.cond, Rect::centered(c.cx(), c.y + 36.0, 28.0, 28.0), hr.night);
            ui.p.text(
                &format!("{:.0}°", hr.temp),
                Rect::new(c.x, c.y + 54.0, c.w, 20.0),
                &TextStyle::new(15.0, palette::TEXT).semibold().center(),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Condition icons: line art in the app's icon style (see `icons::sky`)
// ---------------------------------------------------------------------------

fn draw_condition(p: &mut Painter, cond: Cond, r: Rect, night: bool) {
    let sky = match cond {
        Cond::Clear => Sky::Clear,
        Cond::PartlyCloudy => Sky::PartlyCloudy,
        Cond::Cloudy => Sky::Cloudy,
        Cond::Fog => Sky::Fog,
        Cond::Drizzle => Sky::Drizzle,
        Cond::Rain => Sky::Rain,
        Cond::Snow => Sky::Snow,
        Cond::Storm => Sky::Storm,
    };
    icons::sky(p, sky, r, night, Color::white(0.92), palette::BLUE);
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

struct Place {
    name: String,
    lat: f64,
    lon: f64,
}

fn worker(rx: Receiver<Cmd>, bus: Bus) {
    let mut place: Option<(String, Place)> = None;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Quit => return,
            Cmd::Fetch { city, imperial } => {
                // only the newest request matters
                let (mut city, mut imperial) = (city, imperial);
                while let Ok(c) = rx.try_recv() {
                    match c {
                        Cmd::Quit => return,
                        Cmd::Fetch { city: c2, imperial: i2 } => {
                            city = c2;
                            imperial = i2;
                        }
                    }
                }
                let res = (|| {
                    if place.as_ref().is_none_or(|(c, _)| *c != city) {
                        place = Some((city.clone(), geocode(&city)?));
                    }
                    let p = &place.as_ref().ok_or("no place")?.1;
                    forecast(p, imperial)
                })();
                match res {
                    Ok(f) => bus.to_module(ID, WMsg::Data(f)),
                    Err(e) => bus.to_module(ID, WMsg::Error(e)),
                }
            }
        }
    }
}

fn get_json(url: &str) -> Result<Value, String> {
    let cancel = AtomicBool::new(false);
    let headers = [("User-Agent", "DynamicNotch/0.1".to_string())];
    let (status, body) = http::fetch("GET", url, &headers, &[], Timeouts::DEFAULT, 512 * 1024, &cancel)?;
    if status != 200 {
        return Err(format!("weather service returned HTTP {status}"));
    }
    serde_json::from_slice(&body).map_err(|_| "unexpected answer from the weather service".to_string())
}

fn url_encode(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => o.push(b as char),
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}

/// "Springfield, IL" → search "Springfield", prefer the hit whose region or country mentions "IL".
fn geocode(city: &str) -> Result<Place, String> {
    let (name, hint) = match city.split_once(',') {
        Some((n, h)) => (n.trim(), h.trim().to_lowercase()),
        None => (city.trim(), String::new()),
    };
    let url = format!(
        "https://geocoding-api.open-meteo.com/v1/search?name={}&count=10&language=en&format=json",
        url_encode(name)
    );
    let v = get_json(&url)?;
    let results = v.get("results").and_then(Value::as_array).ok_or_else(|| format!("couldn't find “{city}”"))?;
    let text = |r: &Value, k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").to_lowercase();
    let pick = results
        .iter()
        .find(|r| {
            !hint.is_empty()
                && (text(r, "admin1").contains(&hint)
                    || text(r, "country").contains(&hint)
                    || text(r, "country_code") == hint)
        })
        .or_else(|| results.first())
        .ok_or_else(|| format!("couldn't find “{city}”"))?;
    Ok(Place {
        name: pick.get("name").and_then(Value::as_str).unwrap_or(name).to_string(),
        lat: pick.get("latitude").and_then(Value::as_f64).ok_or("bad location")?,
        lon: pick.get("longitude").and_then(Value::as_f64).ok_or("bad location")?,
    })
}

fn forecast(p: &Place, imperial: bool) -> Result<Forecast, String> {
    let units = if imperial { "temperature_unit=fahrenheit&wind_speed_unit=mph" } else { "wind_speed_unit=kmh" };
    let url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}\
         &current=temperature_2m,apparent_temperature,relative_humidity_2m,weather_code,wind_speed_10m,is_day\
         &daily=weather_code,temperature_2m_max,temperature_2m_min,precipitation_probability_max\
         &hourly=temperature_2m,weather_code,is_day\
         &timezone=auto&forecast_days=5&{units}",
        p.lat, p.lon
    );
    let v = get_json(&url)?;
    let num = |path: &str| v.pointer(path).and_then(Value::as_f64).map(|x| x as f32);
    let cur = |k: &str| num(&format!("/current/{k}"));
    let temp = cur("temperature_2m").ok_or("no temperature in the answer")?;
    let times = v.pointer("/daily/time").and_then(Value::as_array).ok_or("no forecast in the answer")?;
    let arr = |k: &str| v.pointer(&format!("/daily/{k}")).and_then(Value::as_array).cloned().unwrap_or_default();
    let (codes, his, los, rains) = (
        arr("weather_code"),
        arr("temperature_2m_max"),
        arr("temperature_2m_min"),
        arr("precipitation_probability_max"),
    );
    let mut days = Vec::new();
    for (i, t) in times.iter().enumerate() {
        let date = t.as_str().unwrap_or("");
        let f = |a: &Vec<Value>| a.get(i).and_then(Value::as_f64).map(|x| x as f32);
        days.push(Day {
            label: if i == 0 { "Today".into() } else { weekday_label(date) },
            cond: Cond::from_code(codes.get(i).and_then(Value::as_u64).unwrap_or(3) as u32),
            hi: f(&his).unwrap_or(temp),
            lo: f(&los).unwrap_or(temp),
            rain: f(&rains),
        });
    }
    // the next eight hours, starting with the current one (the times are local, "2026-11-08T17:00")
    let hourly = |k: &str| v.pointer(&format!("/hourly/{k}")).and_then(Value::as_array).cloned().unwrap_or_default();
    let (h_times, h_temps, h_codes, h_day) =
        (hourly("time"), hourly("temperature_2m"), hourly("weather_code"), hourly("is_day"));
    let now_hour = v.pointer("/current/time").and_then(Value::as_str).and_then(|t| t.get(..13)).unwrap_or("").to_string();
    let mut hours = Vec::new();
    for (i, t) in h_times.iter().enumerate() {
        let t = t.as_str().unwrap_or("");
        let Some(head) = t.get(..13) else { continue };
        if head < now_hour.as_str() {
            continue;
        }
        let Some(temp) = h_temps.get(i).and_then(Value::as_f64) else { continue };
        hours.push(Hour {
            hour: t.get(11..13).and_then(|x| x.parse().ok()).unwrap_or(0),
            cond: Cond::from_code(h_codes.get(i).and_then(Value::as_u64).unwrap_or(3) as u32),
            night: h_day.get(i).and_then(Value::as_f64).is_some_and(|x| x < 0.5),
            temp: temp as f32,
        });
        if hours.len() == 8 {
            break;
        }
    }
    Ok(Forecast {
        place: p.name.clone(),
        temp,
        feels: cur("apparent_temperature").unwrap_or(temp),
        humidity: cur("relative_humidity_2m").unwrap_or(0.0),
        wind: cur("wind_speed_10m").unwrap_or(0.0),
        cond: Cond::from_code(cur("weather_code").unwrap_or(3.0) as u32),
        night: cur("is_day").is_some_and(|d| d < 0.5),
        days,
        hours,
        imperial,
    })
}

/// "2026-10-24" → "Sat" (Sakamoto's day-of-week algorithm).
fn weekday_label(date: &str) -> String {
    let mut it = date.split('-').filter_map(|x| x.parse::<i32>().ok());
    let (Some(y), Some(m), Some(d)) = (it.next(), it.next(), it.next()) else { return String::new() };
    if !(1..=12).contains(&m) {
        return String::new();
    }
    const T: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let y = if m < 3 { y - 1 } else { y };
    let w = (y + y / 4 - y / 100 + y / 400 + T[(m - 1) as usize] + d).rem_euclid(7);
    ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][w as usize].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weekdays() {
        assert_eq!(weekday_label("2026-10-23"), "Fri");
        assert_eq!(weekday_label("2000-01-01"), "Sat");
        assert_eq!(weekday_label("junk"), "");
    }

    #[test]
    fn codes() {
        assert_eq!(Cond::from_code(0), Cond::Clear);
        assert_eq!(Cond::from_code(63), Cond::Rain);
        assert_eq!(Cond::from_code(95), Cond::Storm);
    }

    #[test]
    fn encoding() {
        assert_eq!(url_encode("São Paulo"), "S%C3%A3o%20Paulo");
    }
}
